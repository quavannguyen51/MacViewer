//! UltraViewerHostSession.swift I/O: command channel, data relay, terminal,
//! and the capture/input workers around the `Core` state machine.

use super::backend::{self, Backend, CaptureJob, InputJob};
use super::encode::{self, Frame};
use super::layout::{Layout, Selection};
use super::login::{self, RelayAddress, RelayRequest};
use super::session::{Action, Core, FramePayload, Pacing, Route};
use super::tools::Tools;
use super::Options;
use crate::command::CommandConnection;
use crate::messages::Registration;
use crate::tls::{self, FrameWriter};
use crate::wire::{self, Envelope, Result};
use std::io::BufRead;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const RELAY_PORT: u16 = 443;

/// A frame waiting to be compressed.
enum EncodeJob {
    Frame { route: Route, sequence: i32, payload: FramePayload },
}

/// Compresses the screen frames on its own thread, so the host keeps
/// answering the viewer while zstd works.
fn spawn_encoder(events: mpsc::Sender<Event>, dictionary: bool) -> mpsc::Sender<EncodeJob> {
    let (jobs, incoming) = mpsc::channel::<EncodeJob>();
    let _ = std::thread::Builder::new().name("linuxviewer-encoder".into()).spawn(move || {
        while let Ok(job) = incoming.recv() {
            let EncodeJob::Frame { route, sequence, payload } = job;
            let dict = dictionary.then_some(crate::screen::SCREEN_DICTIONARY);
            let started = std::time::Instant::now();
            let data = match payload {
                FramePayload::Full(frame) => encode::full_frame_with(&frame, sequence, encode::Compression::Zstandard, dict),
                FramePayload::Delta(delta) => encode::compress_delta(&delta, sequence, dict),
            };
            let took = started.elapsed().as_millis() as u64;
            match data {
                Ok(bytes) => {
                    if events.send(Event::Encoded(route, bytes, took)).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    eprintln!("Screen encoding failed: {e}");
                    return;
                }
            }
        }
    });
    jobs
}

pub(crate) enum Event {
    Command(Vec<u8>),
    CommandClosed(String),
    Relay(u64, Vec<u8>),
    RelayClosed(u64, String),
    Line(String),
    StdinClosed,
    Captured(u64, Result<Frame>),
    /// A screen frame that the encoder thread compressed, and how long that
    /// took in milliseconds.
    Encoded(Route, Vec<u8>, u64),
    Interrupt,
    /// The clipboard watcher has new local text for the viewer.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    ToolsWake,
}

fn system_tag(p: &[u8]) -> &[u8] {
    let tag = p.split(|&b| b == b':').next().unwrap_or(&[]);
    if !tag.is_empty() && tag.len() <= 3 && tag.iter().all(u8::is_ascii_digit) { tag } else { b"other" }
}

fn spawn_reader(mut reader: tls::FrameReader, tx: mpsc::Sender<Event>, wrap: impl Fn(Vec<u8>) -> Event + Send + 'static,
                closed: impl FnOnce(String) -> Event + Send + 'static) {
    thread::spawn(move || loop {
        match reader.recv() {
            Ok(frame) => {
                if tx.send(wrap(frame)).is_err() {
                    return;
                }
            }
            Err(e) => {
                let _ = tx.send(closed(e.to_string()));
                return;
            }
        }
    });
}

struct Relay {
    generation: u64,
    request: RelayRequest,
    writer: Option<FrameWriter>,
    address: Option<RelayAddress>,
    address_deadline: Option<Instant>,
}

#[derive(Clone, Copy, PartialEq)]
enum Purpose {
    Approve { control: bool },
    Stream,
}

const HELP: &str = "Commands: c = toggle control, m N / m all = share display N or all, x = end the session, \
                    t TEXT = chat, f PATH = send a file, a / r ID = accept / refuse a file.";

/// Register, print the ID and password, and serve viewers until Ctrl-C.
pub fn run(registration: &Registration, master_proof: Option<String>, backend: Backend, opts: &Options) -> Result<()> {
    let (tx, rx) = mpsc::channel::<Event>();
    eprintln!("Getting this computer's UltraViewer ID…");
    let mut connection = CommandConnection::register(registration)?;
    let reply = connection.registration().clone();
    let command = connection.sender();
    {
        let tx = tx.clone();
        thread::spawn(move || loop {
            match connection.recv() {
                Ok(p) => if tx.send(Event::Command(p)).is_err() { return },
                Err(e) => { let _ = tx.send(Event::CommandClosed(e.to_string())); return }
            }
        });
    }
    {
        let tx = tx.clone();
        if let Err(e) = ctrlc::set_handler(move || { let _ = tx.send(Event::Interrupt); }) {
            eprintln!("Ctrl-C handler unavailable: {e}");
        }
    }
    {
        let tx = tx.clone();
        thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                match line {
                    Ok(l) => if tx.send(Event::Line(l)).is_err() { return },
                    Err(_) => break,
                }
            }
            let _ = tx.send(Event::StdinClosed);
        });
    }

    let layout = Arc::new(Mutex::new(None::<Layout>));
    let capture = backend::spawn_capture(backend.capturer, layout.clone(), tx.clone(), Event::Captured);
    let encoder = spawn_encoder(tx.clone(), opts.dictionary);
    let input = backend::spawn_input(backend.injector, layout);
    let mut tools = Tools::new(opts, tx.clone());

    let send_command = |text: &str| command.send(text.as_bytes());
    let lists = (reply.ping_list().to_string(), reply.ping_host_name_list().to_string());
    // One stable, parseable line for scripts; the rest is for people.
    println!("ID {} PASSWORD {}", reply.id, registration.password);
    eprintln!("Waiting for a Windows UltraViewer to connect (backend: {}). Ctrl-C to stop.", backend.name);

    let mut core = Core::new(&registration.password, master_proof);
    let pacing = match opts.pacing_ms {
        Some(ms) => {
            let p = Pacing::with_changed_delay(ms);
            eprintln!("Frame pacing: {} ms between frames ({}).", p.changed.as_millis(), if ms == 0 { "as fast as the viewer allows" } else { "custom" });
            p
        }
        None => Pacing::STANDARD,
    };
    core.set_pacing(pacing);
    eprintln!(
        "Screen compression: {}.",
        if opts.dictionary { "with the trained dictionary" } else { "without the dictionary" }
    );
    core.set_dictionary(opts.dictionary);
    if let Some(frames) = opts.frames {
        core.set_frames_in_flight(frames);
    }
    eprintln!("Frames in flight: {}.", core.frames_in_flight_max());
    let mut relay: Option<Relay> = None;
    let mut relay_generation = 0u64;
    let mut next_keepalive = Instant::now() + Duration::from_secs(60);
    let mut selection = opts.selection;
    let mut ticket = 0u64;
    let mut in_flight: Option<(u64, Purpose)> = None;
    let mut stopping = false;
    let mut last_frames_shown = 0u64;
    let mut last_frame_bytes = 0usize;
    let mut capture_started = std::time::Instant::now();
    let mut capture_ms = 0u64;
    let mut compare_ms = 0u64;
    let mut encode_ms = 0u64;

    loop {
        let now = Instant::now();
        let mut wake = next_keepalive;
        for t in [core.next_wake(), relay.as_ref().and_then(|r| r.address_deadline)].into_iter().flatten() {
            wake = wake.min(t);
        }
        // An auto-approved connection must not wait for the next event: the
        // first frame is needed to complete the approval.
        if in_flight.is_none() && core.needs_approval_frame() {
            if let Some(control) = opts.auto_approve {
                core.mark_approval_frame();
                ticket += 1;
                in_flight = Some((ticket, Purpose::Approve { control }));
                let _ = capture.send(CaptureJob { ticket, selection });
            }
        }
        let event = match rx.recv_timeout(wake.saturating_duration_since(now)) {
            Ok(ev) => Some(ev),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err("host event channel closed".into()),
        };
        let now = Instant::now();
        let mut request_capture = |purpose: Purpose, in_flight: &mut Option<(u64, Purpose)>, selection: Selection| {
            ticket += 1;
            *in_flight = Some((ticket, purpose));
            let _ = capture.send(CaptureJob { ticket, selection });
        };
        // Everything that is ready is handled in one turn, but the captured
        // frame goes last: encoding it is the expensive part, and a click
        // that already arrived must not wait behind zstd.
        let mut events: Vec<Event> = event.into_iter().collect();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        events.sort_by_key(|e| matches!(e, Event::Captured(..)));
        for event in events {
            match event {
                Event::Command(payload) => {
                    let payload = payload.as_slice();
                    let tag = system_tag(payload);
                    if tag == b"11" || tag == b"17" {
                        let Some(request) = login::relay_request(payload, &lists.0, &lists.1) else {
                            eprintln!("Ignored a malformed relay request.");
                            continue;
                        };
                        if let Some(r) = relay.as_ref().filter(|r| r.request == request && r.address.is_some()) {
                            send_command(&login::availability(&r.request, r.address.unwrap()))?;
                        } else if !core.has_viewer() {
                            if let Some(old) = relay.take() {
                                if let Some(w) = old.writer { w.shutdown(); }
                            }
                            relay_generation += 1;
                            eprintln!("Preparing the incoming Windows connection…");
                            relay = Some(open_relay(&request, relay_generation, &tx));
                        }
                    }
                }
                Event::CommandClosed(e) => {
                    core.end_viewer("The host stopped.");
                    flush(&mut core, &relay, &input, &mut tools, opts, &encoder);
                    return Err(format!("The UltraViewer command channel closed: {e}").into());
                }
                Event::Relay(generation, bytes) => {
                    let Some(r) = relay.as_mut().filter(|r| r.generation == generation) else { continue };
                    match wire::decode_envelope(&bytes) {
                        Ok(Envelope::System(p)) if p.starts_with(b"0:") => {
                            if let Some(address) = login::relay_address(p) {
                                r.address = Some(address);
                                r.address_deadline = None;
                                send_command(&login::availability(&r.request, address))?;
                                eprintln!("Waiting for the Windows computer's login…");
                            }
                        }
                        Ok(Envelope::Forward { ip, port, payload }) => {
                            let unicode = bytes[1] != 0;
                            if let Some(decoded) = login::peer_payload(payload, unicode) {
                                core.on_peer(&decoded, Route { address: ip, port }, now)?;
                            }
                        }
                        _ => {}
                    }
                }
                Event::RelayClosed(generation, e) => {
                    if relay.as_ref().is_some_and(|r| r.generation == generation) {
                        relay = None;
                        core.end_viewer(&format!("The incoming relay disconnected: {e}"));
                    }
                }
                Event::Encoded(route, bytes, took) => {
                if let Some(w) = relay.as_ref().and_then(|r| r.writer.as_ref()) {
                    send_peer(w, route, &bytes);
                }
                last_frame_bytes = bytes.len();
                encode_ms = took;
            }
            Event::Captured(t, result) => {
                    // Hand the input that already arrived to the desktop before
                    // encoding this frame: zstd must not delay a click.
                    flush(&mut core, &relay, &input, &mut tools, opts, &encoder);
                    if let Some((_, purpose)) = in_flight.filter(|(id, _)| *id == t) {
                        in_flight = None;
                        match (purpose, result) {
                            (Purpose::Approve { control }, Ok(frame)) if core.awaiting_approval() => core.approve(frame, control, now)?,
                            (Purpose::Stream, Ok(frame)) => {
                                let started = std::time::Instant::now();
                                core.submit_frame(frame, now)?;
                                compare_ms = started.elapsed().as_millis() as u64;
                                capture_ms = capture_started.elapsed().as_millis() as u64;
                            }
                            (_, Err(e)) if core.has_viewer() => {
                                core.end_viewer(&format!("Screen capture failed: {e}"));
                                if selection != Selection::All {
                                    eprintln!("Tip: the selected display may be gone; type m all to share every display.");
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Event::Line(line) => {
                    let answer = line.trim();
                    let lower = answer.to_ascii_lowercase();
                    if core.awaiting_approval() {
                        match lower.as_str() {
                            "y" | "yes" | "v" => request_capture(Purpose::Approve { control: lower != "v" }, &mut in_flight, selection),
                            "n" | "no" => core.decline(),
                            _ => eprintln!("Answer y (allow control), v (view only) or n (refuse)."),
                        }
                    } else if let Some(rest) = lower.strip_prefix("m ") {
                        match Selection::parse(rest.trim()) {
                            Ok(s) => {
                                selection = s;
                                eprintln!("Sharing {}.", match s { Selection::All => "all displays".into(), Selection::Display(n) => format!("display {n}") });
                            }
                            Err(e) => eprintln!("{e}"),
                        }
                    } else if core.has_viewer() {
                        match lower.as_str() {
                            "c" => {
                                core.set_control_allowed(!core.control_allowed());
                                eprintln!("Control is {}.", if core.control_allowed() { "allowed" } else { "off (view only)" });
                            }
                            "x" | "q" => core.end_viewer("You ended the incoming session."),
                            "" => {}
                            _ => if !tools.command(answer, core.tools_route()) { eprintln!("{HELP}") },
                        }
                    } else if !answer.is_empty() {
                        eprintln!("No viewer is connected. m N / m all chooses the shared display.");
                    }
                }
                Event::StdinClosed => {}
                Event::Interrupt => {
                    if stopping {
                        std::process::exit(130);
                    }
                    stopping = true;
                    eprintln!("\nStopping…");
                    core.end_viewer("The host stopped sharing.");
                }
                Event::ToolsWake => {}
            }
        }

        if relay.as_ref().is_some_and(|r| r.address_deadline.is_some_and(|t| t <= now)) {
            if let Some(w) = relay.take().and_then(|r| r.writer) { w.shutdown(); }
            core.end_viewer("The incoming relay did not provide an address. Ask Windows to connect again.");
        }
        if now >= next_keepalive {
            next_keepalive = now + Duration::from_secs(60);
            command.keepalive()?;
            if let Some(w) = relay.as_ref().and_then(|r| r.writer.as_ref()) {
                let _ = w.send(&wire::relay_keepalive_envelope());
            }
        }
        core.tick(now);
        // Show how much the last frame cost, so the delta is visible.
        if core.frames_sent != last_frames_shown {
            last_frames_shown = core.frames_sent;
            eprintln!(
                "Frame {}: {:.1} KB (capture {capture_ms} ms, compare {compare_ms} ms, compress {encode_ms} ms)",
                core.frames_sent,
                last_frame_bytes as f64 / 1024.0
            );
        }
        if in_flight.is_none() && core.take_capture_due(now) {
            capture_started = std::time::Instant::now();
            request_capture(Purpose::Stream, &mut in_flight, selection);
        }
        // Auto approval needs the first frame, like the manual answer does.
        if in_flight.is_none() && core.needs_approval_frame() {
            if let Some(control) = opts.auto_approve {
                core.mark_approval_frame();
                request_capture(Purpose::Approve { control }, &mut in_flight, selection);
            }
        }

        flush(&mut core, &relay, &input, &mut tools, opts, &encoder);
        if let (Some(route), Some(w)) = (core.tools_route(), relay.as_ref().and_then(|r| r.writer.as_ref())) {
            for payload in tools.outgoing(route) {
                send_peer(w, route, &payload);
            }
        }

        if stopping {
            if let Some(w) = relay.take().and_then(|r| r.writer) { w.shutdown(); }
            command.shutdown();
            // Let the input worker release held keys before exiting.
            drop(input);
            thread::sleep(Duration::from_millis(100));
            return Ok(());
        }
    }
}

fn send_peer(w: &FrameWriter, route: Route, bytes: &[u8]) {
    if let Err(e) = w.send(&wire::forward_envelope(route.address, route.port, false, bytes)) {
        eprintln!("Relay send failed: {e}");
    }
}

fn flush(core: &mut Core, relay: &Option<Relay>, input: &mpsc::Sender<InputJob>, tools: &mut Tools, opts: &Options,
         encoder: &mpsc::Sender<EncodeJob>) {
    let writer = relay.as_ref().and_then(|r| r.writer.as_ref());
    for action in std::mem::take(&mut core.out) {
        match action {
            Action::Peer(route, bytes) => {
                if let Some(w) = writer {
                    send_peer(w, route, &bytes);
                }
            }
            Action::Frame { route, sequence, payload } => {
                // The compression runs on the encoder thread.
                let _ = encoder.send(EncodeJob::Frame { route, sequence, payload });
            }
            Action::Status(s) => eprintln!("{s}"),
            Action::Prompt(r) => {
                let name = r.display_name.as_deref().unwrap_or("Unknown");
                println!("REQUEST {} {name}", r.viewer_id);
                match opts.auto_approve {
                    Some(control) => eprintln!("Auto-approving {name} ({}).", if control { "view and control" } else { "view only" }),
                    None => eprintln!("Incoming connection from {name} (claims ID {}).\n\
                                       Allow? [y] view and control / [v] view only / [n] refuse (45 s):", r.viewer_id),
                }
            }
            Action::Connected(r) => {
                println!("CONNECTED {}", r.viewer_id);
                eprintln!("{HELP}");
                tools.connected();
            }
            Action::Input(ev) => {
                let _ = input.send(InputJob::Event(ev));
            }
            Action::Tool(payload) => {
                if let Some(route) = core.tools_route() {
                    tools.incoming(&payload, route);
                }
            }
            Action::ReleaseInput => {
                let _ = input.send(InputJob::Release);
            }
            Action::Ended(reason) => {
                println!("ENDED {reason}");
                tools.reset();
            }
        }
    }
}

fn open_relay(request: &RelayRequest, generation: u64, tx: &mpsc::Sender<Event>) -> Relay {
    let mut relay = Relay { generation, request: request.clone(), writer: None, address: None,
                            address_deadline: Some(Instant::now() + Duration::from_secs(15)) };
    match tls::open(&request.address.to_string(), RELAY_PORT, &request.hostname) {
        Ok((reader, writer)) => {
            spawn_reader(reader, tx.clone(), move |f| Event::Relay(generation, f),
                         move |e| Event::RelayClosed(generation, e));
            if let Err(e) = writer.send(&wire::system_envelope(b"0", false)) {
                let _ = tx.send(Event::RelayClosed(generation, e.to_string()));
            }
            relay.writer = Some(writer);
        }
        Err(e) => {
            let _ = tx.send(Event::RelayClosed(generation, e.to_string()));
        }
    }
    relay
}
