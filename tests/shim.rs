use std::fs::{self, File};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Stand-in for the real cargo: logs its args and how many builds are running at once.
const FAKE_CARGO: &str = r#"#!/bin/sh
echo "$*" >> "$LOG"
mkdir "$RUN/$$"
ls "$RUN" | wc -l >> "$PEAK"
sleep 0.5
rmdir "$RUN/$$"
"#;

fn finishes(mut child: Child, within: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < within {
        if let Some(status) = child.try_wait().unwrap() {
            return status.success();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    child.kill().unwrap();
    false
}

#[test]
fn builds_beyond_max_cargo_queue() {
    let root = std::env::temp_dir().join(format!("yogan-shim-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let (shim, fake, permits, run) = (
        root.join("shim"),
        root.join("fake"),
        root.join("permits"),
        root.join("run"),
    );
    for dir in [&shim, &fake, &permits, &run] {
        fs::create_dir_all(dir).unwrap();
    }
    symlink(env!("CARGO_BIN_EXE_yogan"), shim.join("cargo")).unwrap();
    fs::write(fake.join("cargo"), FAKE_CARGO).unwrap();
    fs::set_permissions(fake.join("cargo"), fs::Permissions::from_mode(0o755)).unwrap();
    let (log, peak) = (root.join("log"), root.join("peak"));
    let cargo = |args: &[&str]| {
        Command::new(shim.join("cargo"))
            .args(args)
            .env(
                "PATH",
                format!("{}:{}:/usr/bin:/bin", shim.display(), fake.display()),
            )
            .env("YOGAN_SHIM_DIR", &shim)
            .env("YOGAN_PERMITS", &permits)
            .env("YOGAN_MAX_CARGO", "2")
            .env("YOGAN_BUILD_LOCK", root.join("1.build"))
            .env_remove("YOGAN_CARGO_WRAPPER")
            .envs([("LOG", &log), ("RUN", &run), ("PEAK", &peak)])
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    };
    let read = |p: &Path| fs::read_to_string(p).unwrap_or_default();

    // three builds, two permits: never more than two at once
    let builds: Vec<Child> = (0..3).map(|_| cargo(&["build"])).collect();
    for b in builds {
        assert!(finishes(b, Duration::from_secs(10)));
    }
    let peaks: Vec<u32> = read(&peak)
        .lines()
        .map(|l| l.trim().parse().unwrap())
        .collect();
    assert_eq!(peaks.len(), 3);
    assert_eq!(peaks.iter().max(), Some(&2), "peaks {peaks:?}");

    // tests compile under a permit, then run outside it
    fs::remove_file(&log).unwrap();
    assert!(finishes(
        cargo(&["test", "-p", "x", "--", "--nocapture"]),
        Duration::from_secs(10)
    ));
    assert_eq!(
        read(&log),
        "test --no-run -p x -- --nocapture\ntest -p x -- --nocapture\n"
    );

    // read-only subcommands skip the queue, even with every permit taken
    let held: Vec<File> = (0..2)
        .map(|k| {
            let f = File::create(permits.join(format!("{k}.lock"))).unwrap();
            f.try_lock().unwrap();
            f
        })
        .collect();
    assert!(finishes(
        cargo(&["metadata", "--no-deps"]),
        Duration::from_secs(5)
    ));
    assert!(
        !finishes(cargo(&["build"]), Duration::from_millis(800)),
        "build ran without a permit"
    );
    drop(held);

    fs::remove_dir_all(&root).unwrap();
}
