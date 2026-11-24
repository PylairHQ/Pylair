    // v3-crash fixture (update acceptance): die shortly after startup
    // setup ran — the gate has written its boot marker by then, but the
    // app never reaches ready. The watchdog sees N boot markers with no
    // ready/failed/progress and rolls back crash_loop after max_boots.
    // The Python side backs this up with an on_startup hook that sleeps
    // forever, so no ready marker can land even on a slow clock.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        std::process::exit(3);
    });
