/*
 * mousekeys - A Linux daemon for keyboard-driven mouse control
 * 
 * This program converts numpad keys into mouse movements and clicks, providing
 * accessibility features similar to X11's MouseKeys but implemented as a 
 * standalone daemon using Linux evdev.
 * 
 * FEATURES:
 * - Numpad 1-9: Mouse movement in 8 directions (diagonal movement supported)
 * - Numpad 5: Single click with active mouse button
 * - Numpad +: Double click with active mouse button  
 * - Numpad 0: Mouse button down (drag start)
 * - Numpad .: Mouse button up (drag end)
 * - Numpad /: Select left mouse button
 * - Numpad *: Select middle mouse button
 * - Numpad -: Select right mouse button
 * - Shift: Slow movement mode
 * - Ctrl: Fast movement mode
 * - NumLock: Toggle mousekeys on/off (configurable)
 * - Automatic keyboard detection and switching when new keyboards are plugged in
 * 
 * REQUIREMENTS:
 * - Root privileges (sudo) to access keyboard devices and create virtual devices
 * - Linux with evdev support (/dev/input/event* devices)
 * - Physical keyboard with numpad
 * 
 * CONFIGURATION:
 * - Config file: mousekeys.conf (optional)
 * - Supports speed adjustment, timing, and behavior customization
 * 
 * TECHNICAL DETAILS:
 * - Grabs physical keyboard to intercept numpad events
 * - Creates virtual mouse and keyboard devices for output
 * - Uses exponential smoothing for natural mouse acceleration
 * - Multi-threaded design
 * - Automatic device reconnection on disconnect/reconnect
 * - Dynamic keyboard detection and priority-based switching
 * - Automatic keyboard selection (prefers external over built-in)
 * 
 * USAGE:
 *   sudo ./mousekeys [config_file]
 * 
 * Press Ctrl+C or send SIGTERM to shut down gracefully.
 * 
 * Authors: Claude @ Anthropic, ChatGPT @ OpenAI, JRPData @ github
 * License: MIT License
 */

use evdev::{Device as EvDevice, EventType, KeyCode, InputEvent};
use std::{
    io::{BufRead, Write},
    fs,
    path::PathBuf,
    sync::{Arc, Mutex, mpsc, atomic::{AtomicBool, Ordering}},
    thread::{self, sleep, JoinHandle},
    time::{Duration, Instant},
    panic,
    collections::HashSet,
};
use anyhow::{Result, Context};
use signal_hook::{consts::signal::*, iterator::Signals};

// --- Configuration ---
#[derive(Debug, Clone)]
struct Config {
    vmax_normal: i32,
    vmax_fast: i32,
    vmax_slow: i32,
    tau: f32,
    double_click_delay: u64,
    mousekeys_numlock_on: bool,
    movement_update_rate: u64, // milliseconds
    reconnect_delay: u64,      // seconds
    max_velocity: f32,         // prevent runaway velocity
    keyboard_scan_interval: u64, // seconds - how often to scan for new keyboards
}

impl Default for Config {
    fn default() -> Self {
        Self {
            vmax_normal: 10,
            vmax_fast: 30,
            vmax_slow: 1,
            tau: 0.2,
            double_click_delay: 25,
            mousekeys_numlock_on: true,
            movement_update_rate: 10,
            reconnect_delay: 2,
            max_velocity: 100.0,
            keyboard_scan_interval: 3, // Check for new keyboards every 3 seconds
        }
    }
}

impl Config {
    fn load_from_file(path: &str) -> Self {
        let mut cfg = Config::default();
        if let Ok(file) = fs::File::open(path) {
            for line in std::io::BufReader::new(file).lines().flatten() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') { continue; }
                let parts: Vec<&str> = line.split('=').map(|s| s.trim()).collect();
                if parts.len() != 2 { continue; }
                match parts[0] {
                    "vmax_normal" => cfg.vmax_normal = parts[1].parse().unwrap_or(cfg.vmax_normal),
                    "vmax_fast" => cfg.vmax_fast = parts[1].parse().unwrap_or(cfg.vmax_fast),
                    "vmax_slow" => cfg.vmax_slow = parts[1].parse().unwrap_or(cfg.vmax_slow),
                    "tau" => cfg.tau = parts[1].parse().unwrap_or(cfg.tau),
                    "double_click_delay" => cfg.double_click_delay = parts[1].parse().unwrap_or(cfg.double_click_delay),
                    "mousekeys_numlock_on" => cfg.mousekeys_numlock_on = parts[1].parse().unwrap_or(cfg.mousekeys_numlock_on),
                    "movement_update_rate" => cfg.movement_update_rate = parts[1].parse().unwrap_or(cfg.movement_update_rate),
                    "reconnect_delay" => cfg.reconnect_delay = parts[1].parse().unwrap_or(cfg.reconnect_delay),
                    "max_velocity" => cfg.max_velocity = parts[1].parse().unwrap_or(cfg.max_velocity),
                    "keyboard_scan_interval" => cfg.keyboard_scan_interval = parts[1].parse().unwrap_or(cfg.keyboard_scan_interval),
                    _ => {}
                }
            }
        }
        cfg
    }

    fn validate(&self) -> Result<()> {
        if self.vmax_normal < 0 || self.vmax_fast < 0 || self.vmax_slow < 0 {
            return Err(anyhow::anyhow!("Velocity values must be non-negative"));
        }
        if self.tau <= 0.0 || self.tau > 1.0 {
            return Err(anyhow::anyhow!("Tau must be between 0.0 and 1.0"));
        }
        if self.max_velocity <= 0.0 {
            return Err(anyhow::anyhow!("Max velocity must be positive"));
        }
        // keyboard_scan_interval of 0 disables scanning, so it's valid
        Ok(())
    }
}

// --- Keyboard candidate information ---
#[derive(Debug, Clone)]
struct KeyboardCandidate {
    path: PathBuf,
    name: String,
    priority: i32,
}

// --- RAII Guard with better error handling ---
struct GrabGuard {
    keyboard: EvDevice,
    grabbed: bool,
}

impl GrabGuard {
    fn new(mut keyboard: EvDevice) -> Result<Self> {
        keyboard.grab().context("Failed to grab keyboard")?;
        Ok(Self { keyboard, grabbed: true })
    }

    fn ungrab(&mut self) -> Result<()> {
        if self.grabbed {
            self.keyboard.ungrab().context("Failed to ungrab keyboard")?;
            self.grabbed = false;
        }
        Ok(())
    }
}

impl Drop for GrabGuard {
    fn drop(&mut self) {
        if let Err(e) = self.ungrab() {
            eprintln!("Warning: Failed to ungrab keyboard on drop: {}", e);
        }
    }
}

// --- Mouse & State ---
#[derive(Debug, Clone, Copy)]
enum MouseButton { Left, Middle, Right }

#[derive(Debug)]
struct MouseKeyState {
    active_button: MouseButton,
    mousekeys_enabled: bool,
    mouse_enabled_when_numlock_on: bool,
    last_numlock_check: Instant,
}

impl MouseKeyState {
    fn new(numlock_on: bool) -> Self {
        Self {
            active_button: MouseButton::Left,
            mousekeys_enabled: numlock_on,
            mouse_enabled_when_numlock_on: numlock_on,
            last_numlock_check: Instant::now(),
        }
    }

    fn evdev_button(&self) -> KeyCode {
        match self.active_button {
            MouseButton::Left => KeyCode::BTN_LEFT,
            MouseButton::Middle => KeyCode::BTN_MIDDLE,
            MouseButton::Right => KeyCode::BTN_RIGHT,
        }
    }
}

// --- Modifiers ---
#[derive(Default, Debug, Clone, Copy)]
struct Modifiers {
    shift: bool,
    ctrl: bool,
}

impl Modifiers {
    fn effective_vmax(&self, cfg: &Config) -> i32 {
        if self.ctrl { cfg.vmax_fast }
        else if self.shift { cfg.vmax_slow }
        else { cfg.vmax_normal }
    }
}

// --- Mouse movement state ---
#[derive(Debug, Default)]
struct MouseMoveState {
    pressed_keys: HashSet<KeyCode>, // KP1-KP9 (no KP5)
    active: bool,
    modifiers: Modifiers,
    velocity_x: f32,
    velocity_y: f32,
}

impl MouseMoveState {
    fn clamp_velocity(&mut self, max_vel: f32) {
        self.velocity_x = self.velocity_x.clamp(-max_vel, max_vel);
        self.velocity_y = self.velocity_y.clamp(-max_vel, max_vel);
    }
}

// --- Async mouse tasks ---
enum MouseTask {
    DelayedClick(Vec<InputEvent>, Duration),
}

// --- Keyboard switch signal ---
enum KeyboardCommand {
    SwitchTo(KeyboardCandidate),
}

// --- Thread-safe shutdown coordination ---
struct ShutdownCoordinator {
    shutdown: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl ShutdownCoordinator {
    fn new() -> Self {
        Self {
            shutdown: Arc::new(AtomicBool::new(false)),
            threads: Vec::new(),
        }
    }

    fn should_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }

    fn trigger_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    fn add_thread(&mut self, handle: JoinHandle<()>) {
        self.threads.push(handle);
    }

    fn wait_for_shutdown(self, _timeout: Duration) -> Result<()> {
        for handle in self.threads {
            if let Err(e) = handle.join() {
                eprintln!("Thread panicked: {:?}", e);
            }
        }
        Ok(())
    }
}

// --- Main ---
fn main() -> Result<()> {
    // Check for root privileges first
    check_root_privileges()?;
    
    let cfg = Config::load_from_file("mousekeys.conf");
    cfg.validate().context("Invalid configuration")?;
    println!("Loaded config: {:?}", cfg);

    // Set up panic hook for cleaner shutdown
    let original_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        eprintln!("PANIC: {:?}", panic_info);
        original_hook(panic_info);
    }));

    let result = panic::catch_unwind(|| run_mousekeys(cfg));
    match result {
        Ok(r) => r,
        Err(_) => {
            eprintln!("Program panicked. Attempting cleanup...");
            Err(anyhow::anyhow!("Program panicked"))
        }
    }
}

// --- Root privilege check ---
fn check_root_privileges() -> Result<()> {
    // Check if we can access /dev/input directory with proper permissions
    let input_dir = std::path::Path::new("/dev/input");
    
    // Try to read the directory - this will fail if we don't have sufficient privileges
    match fs::read_dir(input_dir) {
        Ok(_) => {
            // Additional check: try to open an event device
            if let Some(test_device) = find_test_device() {
                match EvDevice::open(&test_device) {
                    Ok(mut device) => {
                        // Try to grab the device - this requires root
                        match device.grab() {
                            Ok(_) => {
                                let _ = device.ungrab(); // Clean up
                                println!("✓ Running with sufficient privileges to access input devices");
                                return Ok(());
                            }
                            Err(_) => {
                                // Can open but can't grab - likely permission issue
                            }
                        }
                    }
                    Err(_) => {
                        // Can't open device - permission issue
                    }
                }
            }
        }
        Err(_) => {
            // Can't even read /dev/input directory
        }
    }
    
    // If we get here, we don't have sufficient privileges
    let error_msg = "ERROR: This program requires root privileges to access keyboard devices and create virtual input devices.";
    let solution_msg = "Please run with: sudo ./mousekeys";
    let detail_msg = "Unable to access /dev/input devices or grab keyboard devices.";
    
    // Log to stderr (console)
    eprintln!("{}", error_msg);
    eprintln!("{}", solution_msg);
    eprintln!("{}", detail_msg);
    
    // Log to stdout as well for good measure
    println!("{}", error_msg);
    println!("{}", solution_msg);
    
    // Log to dmesg/kernel log using logger if available
    let dmesg_msg = format!("mousekeys: {} {}", error_msg, solution_msg);
    log_to_kernel(&dmesg_msg);
    
    // Also try to write to syslog
    log_to_syslog(&error_msg, &solution_msg);
    
    Err(anyhow::anyhow!("Insufficient privileges: {}", detail_msg))
}

fn find_test_device() -> Option<PathBuf> {
    let input_dir = std::path::Path::new("/dev/input");
    if let Ok(entries) = fs::read_dir(input_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(filename) = path.file_name().and_then(|f| f.to_str()) {
                if filename.starts_with("event") {
                    return Some(path);
                }
            }
        }
    }
    None
}

fn log_to_kernel(message: &str) {
    // Try to use logger command to write to kernel log
    if let Ok(mut child) = std::process::Command::new("logger")
        .arg("-t")
        .arg("mousekeys")
        .arg("-p")
        .arg("daemon.err")
        .arg(message)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        let _ = child.wait();
    }
    
    // Also try direct write to /dev/kmsg if available
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/kmsg")
    {
        let _ = writeln!(file, "<3>mousekeys: {}", message);
    }
}

fn log_to_syslog(error_msg: &str, solution_msg: &str) {
    // Try to write to syslog using logger
    for (priority, msg) in [("daemon.err", error_msg), ("daemon.info", solution_msg)] {
        if let Ok(mut child) = std::process::Command::new("logger")
            .arg("-t")
            .arg("mousekeys")
            .arg("-p")
            .arg(priority)
            .arg(msg)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            let _ = child.wait();
        }
    }
}

// --- Core run loop with reconnection logic ---
fn run_mousekeys(cfg: Config) -> Result<()> {
    println!("Starting mousekeys daemon...");

    loop {
        match run_mousekeys_session(&cfg) {
            Ok(_) => {
                println!("Session ended normally");
                break;
            }
            Err(e) => {
                eprintln!("Session error: {}. Attempting reconnection in {} seconds...", 
                         e, cfg.reconnect_delay);
                sleep(Duration::from_secs(cfg.reconnect_delay));
            }
        }
    }

    Ok(())
}

fn run_mousekeys_session(cfg: &Config) -> Result<()> {
    // Find initial keyboard and print the list
    let mut keyboard_list_signature = String::new();
    let all_keyboards = find_all_keyboards();
    print_keyboards_if_changed(&all_keyboards, &mut keyboard_list_signature);
    
    let initial_keyboard = all_keyboards.into_iter().next()
        .ok_or_else(|| anyhow::anyhow!("No suitable keyboard found"))?;

    println!("Starting session with keyboard: {} ({})", initial_keyboard.name, initial_keyboard.path.display());

    // Open the initial keyboard device
    let initial_device = EvDevice::open(&initial_keyboard.path)
        .context("Failed to open initial keyboard device")?;

    // Create virtual devices with error handling
    let mut virtual_keyboard = evdev::uinput::VirtualDevice::builder()
        .context("Failed to create virtual keyboard builder")?
        .name("Rust Virtual Keyboard")
        .with_keys(&initial_device.supported_keys().unwrap_or_default())
        .context("Failed to set keyboard keys")?
        .build()
        .context("Failed to build virtual keyboard")?;

    let virtual_mouse = evdev::uinput::VirtualDevice::builder()
        .context("Failed to create virtual mouse builder")?
        .name("Rust Virtual Mouse")
        .with_relative_axes(&evdev::AttributeSet::from_iter([
            evdev::RelativeAxisCode::REL_X,
            evdev::RelativeAxisCode::REL_Y,
        ]))
        .context("Failed to set mouse axes")?
        .with_keys(&evdev::AttributeSet::from_iter([
            KeyCode::BTN_LEFT,
            KeyCode::BTN_MIDDLE,
            KeyCode::BTN_RIGHT,
        ]))
        .context("Failed to set mouse buttons")?
        .build()
        .context("Failed to build virtual mouse")?;

    // Brief delay for device initialization
    sleep(Duration::from_millis(100));

    let keyboard_for_grab = EvDevice::open(&initial_keyboard.path)
        .context("Failed to open keyboard for grabbing")?;
    keyboard_for_grab.set_nonblocking(true)
        .context("Failed to set keyboard non-blocking")?;
    let mut guard = GrabGuard::new(keyboard_for_grab)
        .context("Failed to grab keyboard")?;

    let mut state = MouseKeyState::new(cfg.mousekeys_numlock_on);
    let mut coordinator = ShutdownCoordinator::new();
    let mut current_keyboard = initial_keyboard.clone();

    let mouse_dev = Arc::new(Mutex::new(virtual_mouse));
    let move_state = Arc::new(Mutex::new(MouseMoveState::default()));
    let (tx, rx) = mpsc::channel::<MouseTask>();
    let (kbd_tx, kbd_rx) = mpsc::channel::<KeyboardCommand>();

    // Movement thread with improved error handling
    let move_state_clone = Arc::clone(&move_state);
    let mouse_clone = Arc::clone(&mouse_dev);
    let cfg_clone = cfg.clone();
    let modifiers = Arc::new(Mutex::new(Modifiers::default()));
    let modifiers_clone = Arc::clone(&modifiers);
    let shutdown_clone = Arc::clone(&coordinator.shutdown);

    let movement_handle = thread::spawn(move || {
        let dt = Duration::from_millis(cfg_clone.movement_update_rate);
        while !shutdown_clone.load(Ordering::Relaxed) {
            sleep(dt);
            
            let mut state_lock = match move_state_clone.try_lock() {
                Ok(lock) => lock,
                Err(_) => continue, // Skip if can't acquire lock
            };
            
            if !state_lock.active { continue; }

            let mods = match modifiers_clone.try_lock() {
                Ok(lock) => *lock,
                Err(_) => continue,
            };
            
            let vmax = mods.effective_vmax(&cfg_clone);

            let mut dx = 0;
            let mut dy = 0;
            for &key in &state_lock.pressed_keys {
                match key {
                    KeyCode::KEY_KP1 => { dx -= 1; dy += 1; }
                    KeyCode::KEY_KP2 => { dy += 1; }
                    KeyCode::KEY_KP3 => { dx += 1; dy += 1; }
                    KeyCode::KEY_KP4 => { dx -= 1; }
                    KeyCode::KEY_KP6 => { dx += 1; }
                    KeyCode::KEY_KP7 => { dx -= 1; dy -= 1; }
                    KeyCode::KEY_KP8 => { dy -= 1; }
                    KeyCode::KEY_KP9 => { dx += 1; dy -= 1; }
                    _ => {}
                }
            }

            if mods.ctrl || mods.shift {
                state_lock.velocity_x = dx as f32 * vmax as f32;
                state_lock.velocity_y = dy as f32 * vmax as f32;
            } else {
                let alpha = 1.0 - (-cfg_clone.tau).exp();
                state_lock.velocity_x += (dx as f32 * vmax as f32 - state_lock.velocity_x) * alpha;
                state_lock.velocity_y += (dy as f32 * vmax as f32 - state_lock.velocity_y) * alpha;
            }

            // Clamp velocities to prevent runaway
            state_lock.clamp_velocity(cfg_clone.max_velocity);

            let mod_dx = state_lock.velocity_x.round() as i32;
            let mod_dy = state_lock.velocity_y.round() as i32;

            if mod_dx != 0 || mod_dy != 0 {
                let events = vec![
                    InputEvent::new(EventType::RELATIVE.0, evdev::RelativeAxisCode::REL_X.0, mod_dx),
                    InputEvent::new(EventType::RELATIVE.0, evdev::RelativeAxisCode::REL_Y.0, mod_dy),
                    InputEvent::new(EventType::SYNCHRONIZATION.0, 0, 0),
                ];
                if let Ok(mut dev) = mouse_clone.try_lock() {
                    if let Err(e) = dev.emit(&events) {
                        eprintln!("Failed to emit mouse movement: {}", e);
                    }
                }
            }
        }
    });

    coordinator.add_thread(movement_handle);

    // Async click thread with shutdown handling
    let mouse_for_click = Arc::clone(&mouse_dev);
    let shutdown_clone2 = Arc::clone(&coordinator.shutdown);
    let click_handle = thread::spawn(move || {
        while let Ok(task) = rx.recv() {
            if shutdown_clone2.load(Ordering::Relaxed) { break; }
            
            let MouseTask::DelayedClick(events, delay) = task;
            sleep(delay);
            
            if shutdown_clone2.load(Ordering::Relaxed) { break; }
            
            if let Ok(mut dev) = mouse_for_click.try_lock() {
                if let Err(e) = dev.emit(&events) {
                    eprintln!("Failed to emit mouse click: {}", e);
                }
            }
        }
    });

    coordinator.add_thread(click_handle);

    // Keyboard monitoring thread (only if scanning is enabled)
    if cfg.keyboard_scan_interval > 0 {
        let kbd_tx_for_monitor = kbd_tx.clone();
        let cfg_clone2 = cfg.clone();
        let shutdown_clone3 = Arc::clone(&coordinator.shutdown);
        let current_keyboard_name = initial_keyboard.name.clone();
        let current_keyboard_priority = initial_keyboard.priority;
        let kbd_monitor_handle = thread::spawn(move || {
            let mut last_scan = Instant::now();
            let mut keyboard_list_signature = String::new();
            let scan_interval = Duration::from_secs(cfg_clone2.keyboard_scan_interval);
            
            while !shutdown_clone3.load(Ordering::Relaxed) {
                sleep(Duration::from_millis(500)); // Check every 500ms
                
                if last_scan.elapsed() >= scan_interval {
                    let all_keyboards = find_all_keyboards();
                    let list_changed = print_keyboards_if_changed(&all_keyboards, &mut keyboard_list_signature);
                    
                    if let Some(best_keyboard) = all_keyboards.into_iter().next() {
                        // Check if we found a better keyboard than the current one
                        if best_keyboard.priority > current_keyboard_priority ||
                           (best_keyboard.priority == current_keyboard_priority && best_keyboard.name != current_keyboard_name) {
                            
                            if list_changed {
                                println!("Switching to better keyboard: {} ({}) - Priority: {} (was: {} - Priority: {})",
                                        best_keyboard.name, best_keyboard.path.display(), best_keyboard.priority,
                                        current_keyboard_name, current_keyboard_priority);
                            } else {
                                println!("Found better keyboard: {} ({}) - Priority: {} (current: {} - Priority: {})",
                                        best_keyboard.name, best_keyboard.path.display(), best_keyboard.priority,
                                        current_keyboard_name, current_keyboard_priority);
                            }
                            
                            if let Err(e) = kbd_tx_for_monitor.send(KeyboardCommand::SwitchTo(best_keyboard)) {
                                eprintln!("Failed to send keyboard switch command: {}", e);
                                break;
                            }
                            break; // Exit the monitoring thread after sending switch command
                        }
                    }
                    last_scan = Instant::now();
                }
            }
        });

        coordinator.add_thread(kbd_monitor_handle);
        println!("Keyboard monitoring enabled (scan interval: {} seconds)", cfg.keyboard_scan_interval);
    } else {
        println!("Keyboard monitoring disabled (scan interval set to 0)");
    }

    // Signal handling thread
    let shutdown_clone4 = Arc::clone(&coordinator.shutdown);
    let signal_handle = thread::spawn(move || {
        let mut signals = match Signals::new(&[SIGINT, SIGTERM]) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Failed to set up signal handling: {}", e);
                return;
            }
        };
        
        for sig in signals.forever() {
            println!("Received signal: {}. Shutting down gracefully...", sig);
            shutdown_clone4.store(true, Ordering::Relaxed);
            break;
        }
    });

    coordinator.add_thread(signal_handle);

    // Main event loop
    println!("Entering main event loop...");
    while !coordinator.should_shutdown() {
        // Check for keyboard switch commands
        if let Ok(KeyboardCommand::SwitchTo(new_keyboard)) = kbd_rx.try_recv() {
            println!("Switching to new keyboard: {} ({})", new_keyboard.name, new_keyboard.path.display());
            
            // Ungrab current keyboard
            if let Err(e) = guard.ungrab() {
                eprintln!("Warning: Failed to ungrab current keyboard: {}", e);
            }
            
            // Open and grab new keyboard
            match EvDevice::open(&new_keyboard.path) {
                Ok(new_device) => {
                    if let Err(e) = new_device.set_nonblocking(true) {
                        eprintln!("Failed to set new keyboard non-blocking: {}", e);
                        continue;
                    }
                    
                    match GrabGuard::new(new_device) {
                        Ok(new_guard) => {
                            guard = new_guard;
                            current_keyboard = new_keyboard;
                            
                            // Reset mouse movement state when switching keyboards
                            if let Ok(mut move_state_lock) = move_state.try_lock() {
                                move_state_lock.pressed_keys.clear();
                                move_state_lock.active = false;
                                move_state_lock.velocity_x = 0.0;
                                move_state_lock.velocity_y = 0.0;
                            }
                            
                            println!("Successfully switched to: {} ({})", current_keyboard.name, current_keyboard.path.display());
                        }
                        Err(e) => {
                            eprintln!("Failed to grab new keyboard: {}", e);
                            // Try to re-grab the old keyboard
                            if let Ok(old_device) = EvDevice::open(&current_keyboard.path) {
                                if old_device.set_nonblocking(true).is_ok() {
                                    if let Ok(old_guard) = GrabGuard::new(old_device) {
                                        guard = old_guard;
                                        println!("Reverted to previous keyboard");
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Failed to open new keyboard device: {}", e);
                }
            }
        }

        let events = match guard.keyboard.fetch_events() {
            Ok(evts) => evts,
            Err(e) => {
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    sleep(Duration::from_millis(5));
                    continue;
                } else {
                    return Err(anyhow::anyhow!("Keyboard read error: {}", e));
                }
            }
        };

        let mut numlock_pressed = false;

        for event in events {
            // Check for shutdown between events
            if coordinator.should_shutdown() {
                break;
            }
            
            if event.event_type() == EventType::KEY {
                let code = KeyCode(event.code());
                let value = event.value();

                let mut reset_velocity = false;
                match (code, value) {
                    (KeyCode::KEY_LEFTSHIFT | KeyCode::KEY_RIGHTSHIFT, 1) => {
                        if let Ok(mut mods) = modifiers.try_lock() {
                            mods.shift = true;
                            reset_velocity = true;
                        }
                    }
                    (KeyCode::KEY_LEFTSHIFT | KeyCode::KEY_RIGHTSHIFT, 0) => {
                        if let Ok(mut mods) = modifiers.try_lock() {
                            mods.shift = false;
                            reset_velocity = true;
                        }
                    }
                    (KeyCode::KEY_LEFTCTRL | KeyCode::KEY_RIGHTCTRL, 1) => {
                        if let Ok(mut mods) = modifiers.try_lock() {
                            mods.ctrl = true;
                            reset_velocity = true;
                        }
                    }
                    (KeyCode::KEY_LEFTCTRL | KeyCode::KEY_RIGHTCTRL, 0) => {
                        if let Ok(mut mods) = modifiers.try_lock() {
                            mods.ctrl = false;
                            reset_velocity = true;
                        }
                    }
                    _ => {}
                }

                if reset_velocity {
                    if let (Ok(mut mv), Ok(mods)) = (move_state.try_lock(), modifiers.try_lock()) {
                        mv.modifiers = *mods;
                        mv.velocity_x = 0.0;
                        mv.velocity_y = 0.0;
                    }
                } else {
                    let is_mousekey = matches!(code,
                        KeyCode::KEY_KP1 | KeyCode::KEY_KP2 | KeyCode::KEY_KP3 |
                        KeyCode::KEY_KP4 | KeyCode::KEY_KP6 |
                        KeyCode::KEY_KP7 | KeyCode::KEY_KP8 | KeyCode::KEY_KP9 |
                        KeyCode::KEY_KPSLASH | KeyCode::KEY_KPASTERISK | KeyCode::KEY_KPMINUS |
                        KeyCode::KEY_KP5 | KeyCode::KEY_KPPLUS |
                        KeyCode::KEY_KP0 | KeyCode::KEY_KPDOT
                    );

                    if is_mousekey && state.mousekeys_enabled {
                        if let (Ok(mut mv), Ok(mods)) = (move_state.try_lock(), modifiers.try_lock()) {
                            mv.modifiers = *mods;
                            match value {
                                1 => {
                                    if let Err(e) = handle_key_press(&mouse_dev, &tx, &mut state, code, cfg) {
                                        eprintln!("Error handling key press: {}", e);
                                    }
                                    if matches!(code,
                                        KeyCode::KEY_KP1 | KeyCode::KEY_KP2 | KeyCode::KEY_KP3 |
                                        KeyCode::KEY_KP4 | KeyCode::KEY_KP6 |
                                        KeyCode::KEY_KP7 | KeyCode::KEY_KP8 | KeyCode::KEY_KP9) {
                                        mv.velocity_x = 0.0;
                                        mv.velocity_y = 0.0;
                                        mv.pressed_keys.insert(code);
                                        mv.active = true;
                                    }
                                }
                                0 => {
                                    if let Err(e) = handle_key_release(&mouse_dev, &mut state, code) {
                                        eprintln!("Error handling key release: {}", e);
                                    }
                                    mv.pressed_keys.remove(&code);
                                    if mv.pressed_keys.is_empty() { mv.active = false; }
                                }
                                _ => {}
                            }
                        }
                        continue;
                    }

                    if code == KeyCode::KEY_NUMLOCK && value == 1 { numlock_pressed = true; }
                }
            }
            
            // Forward non-mousekey events
            if let Err(e) = virtual_keyboard.emit(&[event]) {
                eprintln!("Failed to emit keyboard event: {}", e);
            }
        }

        // Handle numlock state changes with rate limiting
        if numlock_pressed && state.last_numlock_check.elapsed() > Duration::from_millis(100) {
            if let Ok(led) = guard.keyboard.get_led_state() {
                let num_on = led.contains(evdev::LedCode::LED_NUML);
                state.mousekeys_enabled = if state.mouse_enabled_when_numlock_on { num_on } else { !num_on };
                
                if let Ok(mut move_state_lock) = move_state.try_lock() {
                    move_state_lock.velocity_x = 0.0;
                    move_state_lock.velocity_y = 0.0;
                }
            }
            state.last_numlock_check = Instant::now();
        }

        // Check for keyboard disconnection
        if !current_keyboard.path.exists() {
            return Err(anyhow::anyhow!("Keyboard device disconnected: {}", current_keyboard.path.display()));
        }
        
        // Brief sleep to prevent busy waiting
        if coordinator.should_shutdown() {
            break;
        }
    }

    println!("Triggering shutdown...");
    coordinator.trigger_shutdown();
    
    // Give threads a moment to shut down gracefully
    sleep(Duration::from_millis(100));
    
    // Close channels to wake up threads
    drop(tx);
    drop(kbd_tx);
    
    println!("Waiting for threads to finish...");
    coordinator.wait_for_shutdown(Duration::from_secs(2))?;
    
    println!("Session ended gracefully");
    Ok(())
}

// --- Find all keyboards and return sorted list ---
fn find_all_keyboards() -> Vec<KeyboardCandidate> {
    let input_dir = std::path::Path::new("/dev/input");
    let entries = match fs::read_dir(input_dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    
    let mut candidates = Vec::new();
    
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(_) => continue,
        };
        
        if let Some(filename) = path.file_name().and_then(|f| f.to_str()) {
            if filename.starts_with("event") {
                if let Ok(device) = EvDevice::open(&path) {
                    if device.set_nonblocking(true).is_ok() {
                        if let Some(keys) = device.supported_keys() {
                            if keys.contains(KeyCode::KEY_A) && keys.contains(KeyCode::KEY_NUMLOCK) {
                                let name = device.name().unwrap_or("Unknown").to_string();
                                let name_lower = name.to_lowercase();
                                
                                // Skip virtual devices
                                if name_lower.contains("virtual") || name_lower.contains("rust") { 
                                    continue; 
                                }
                                
                                // Calculate priority score (higher = better)
                                let priority = calculate_keyboard_priority(&name_lower, keys);
                                
                                candidates.push(KeyboardCandidate {
                                    path,
                                    name,
                                    priority,
                                });
                            }
                        }
                    }
                }
            }
        }
    }
    
    // Sort by priority (highest first)
    candidates.sort_by(|a, b| b.priority.cmp(&a.priority));
    candidates
}

// --- Generate a signature for the current keyboard list ---
fn generate_keyboard_list_signature(keyboards: &[KeyboardCandidate]) -> String {
    keyboards.iter()
        .map(|k| format!("{}:{}", k.path.display(), k.priority))
        .collect::<Vec<_>>()
        .join("|")
}

// --- Print keyboard list if it changed ---
fn print_keyboards_if_changed(keyboards: &[KeyboardCandidate], last_signature: &mut String) -> bool {
    let current_signature = generate_keyboard_list_signature(keyboards);
    
    if current_signature != *last_signature {
        if keyboards.is_empty() {
            println!("No suitable keyboards found");
        } else {
            println!("Available keyboards:");
            for (i, candidate) in keyboards.iter().enumerate() {
                println!("  {}: {} ({}) - Priority: {}", 
                        i, candidate.name, candidate.path.display(), candidate.priority);
            }
            
            if let Some(best) = keyboards.first() {
                println!("Best keyboard: {} ({}) - Priority: {}", 
                        best.name, best.path.display(), best.priority);
            }
        }
        
        *last_signature = current_signature;
        true
    } else {
        false
    }
}

fn calculate_keyboard_priority(name_lower: &str, keys: &evdev::AttributeSetRef<KeyCode>) -> i32 {
    let mut priority = 0;
    
    // Prefer external/USB keyboards
    if name_lower.contains("usb") || name_lower.contains("external") {
        priority += 100;
    }
    
    // Prefer keyboards with "keyboard" in name
    if name_lower.contains("keyboard") {
        priority += 50;
    }
    
    // Deprioritize laptop/built-in keyboards
    if name_lower.contains("laptop") || name_lower.contains("built-in") || 
       name_lower.contains("at translated") || name_lower.contains("atkbd") {
        priority -= 50;
    }
    
    // Check if it has numpad keys (good sign for external keyboard)
    if keys.contains(KeyCode::KEY_KP1) && keys.contains(KeyCode::KEY_KP5) {
        priority += 30;
    }
    
    // Prefer keyboards with more complete numpad
    let numpad_keys = [
        KeyCode::KEY_KP1, KeyCode::KEY_KP2, KeyCode::KEY_KP3,
        KeyCode::KEY_KP4, KeyCode::KEY_KP5, KeyCode::KEY_KP6,
        KeyCode::KEY_KP7, KeyCode::KEY_KP8, KeyCode::KEY_KP9,
        KeyCode::KEY_KP0, KeyCode::KEY_KPDOT, KeyCode::KEY_KPPLUS,
        KeyCode::KEY_KPMINUS, KeyCode::KEY_KPASTERISK, KeyCode::KEY_KPSLASH
    ];
    
    let numpad_count = numpad_keys.iter().filter(|&&key| keys.contains(key)).count();
    priority += numpad_count as i32; // Each numpad key adds 1 point
    
    // Bonus for having all essential mousekey keys
    if numpad_count >= 12 { // Most essential numpad keys present
        priority += 20;
    }
    
    priority
}

fn handle_key_press(
    mouse_dev: &Arc<Mutex<evdev::uinput::VirtualDevice>>,
    tx: &mpsc::Sender<MouseTask>,
    state: &mut MouseKeyState,
    key: KeyCode,
    cfg: &Config,
) -> Result<()> {
    match key {
        KeyCode::KEY_KPSLASH => state.active_button = MouseButton::Left,
        KeyCode::KEY_KPASTERISK => state.active_button = MouseButton::Middle,
        KeyCode::KEY_KPMINUS => state.active_button = MouseButton::Right,

        KeyCode::KEY_KP5 => {
            if let Ok(mut dev) = mouse_dev.try_lock() {
                let events = vec![
                    InputEvent::new(EventType::KEY.0, state.evdev_button().0, 1),
                    InputEvent::new(EventType::SYNCHRONIZATION.0, 0, 0),
                    InputEvent::new(EventType::KEY.0, state.evdev_button().0, 0),
                    InputEvent::new(EventType::SYNCHRONIZATION.0, 0, 0),
                ];
                dev.emit(&events).context("Failed to emit single click")?;
            }
        }

        KeyCode::KEY_KPPLUS => {
            let click_events = vec![
                InputEvent::new(EventType::KEY.0, state.evdev_button().0, 1),
                InputEvent::new(EventType::SYNCHRONIZATION.0, 0, 0),
                InputEvent::new(EventType::KEY.0, state.evdev_button().0, 0),
                InputEvent::new(EventType::SYNCHRONIZATION.0, 0, 0),
            ];
            tx.send(MouseTask::DelayedClick(click_events.clone(), Duration::from_millis(0)))
                .context("Failed to send first click")?;
            tx.send(MouseTask::DelayedClick(click_events, Duration::from_millis(cfg.double_click_delay)))
                .context("Failed to send second click")?;
        }

        KeyCode::KEY_KP0 => {
            if let Ok(mut dev) = mouse_dev.try_lock() {
                let events = vec![
                    InputEvent::new(EventType::KEY.0, state.evdev_button().0, 1),
                    InputEvent::new(EventType::SYNCHRONIZATION.0, 0, 0),
                ];
                dev.emit(&events).context("Failed to emit mouse down")?;
            }
        }

        KeyCode::KEY_KPDOT => {
            if let Ok(mut dev) = mouse_dev.try_lock() {
                let events = vec![
                    InputEvent::new(EventType::KEY.0, state.evdev_button().0, 0),
                    InputEvent::new(EventType::SYNCHRONIZATION.0, 0, 0),
                ];
                dev.emit(&events).context("Failed to emit mouse up")?;
            }
        }

        _ => {}
    }
    Ok(())
}

fn handle_key_release(
    _mouse_dev: &Arc<Mutex<evdev::uinput::VirtualDevice>>,
    _state: &mut MouseKeyState,
    _key: KeyCode,
) -> Result<()> {
    // Currently no action needed on key release for mouse keys
    Ok(())
}