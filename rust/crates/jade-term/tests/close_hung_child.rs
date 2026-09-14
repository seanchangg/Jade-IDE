//! `destroy` must not block the caller, and must stop a foreground program
//! that ignores `SIGHUP` (Claude Code does). Before the reaper thread, the
//! close waited for the shell on the caller's thread, and the shell could not
//! finish its exit while that program held the terminal open.

use jade_term::TermManager;
use std::path::Path;
use std::time::{Duration, Instant};

/// True while a process with `pid` exists.
fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

async fn wait_for_pid(manager: &TermManager, id: u32) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(snap) = manager.snapshot(id) {
            let text: String = snap
                .scrollback
                .iter()
                .chain(snap.cells.iter())
                .map(|row| row.iter().map(|c| c.ch).collect::<String>() + "\n")
                .collect();
            // The echoed command line also contains "pid=$$"; take the line
            // where digits follow.
            for rest in text.split("pid=").skip(1) {
                let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(pid) = digits.parse::<i32>() {
                    if pid > 0 {
                        return pid;
                    }
                }
            }
        }
        assert!(Instant::now() < deadline, "child pid never appeared");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn close_with_hup_ignoring_child(shell: &str) {
    let manager = TermManager::new();
    let id = manager
        .create_with_shell(Path::new("/tmp"), Path::new(shell))
        .expect("PTY available");
    // The foreground job inherits the ignored SIGHUP, like a program that
    // installs its own handler.
    manager.write(id, b"trap '' HUP; sh -c 'echo pid=$$; exec sleep 300'\n");
    let child = wait_for_pid(&manager, id).await;
    assert!(alive(child));

    let t = Instant::now();
    manager.destroy(id);
    let took = t.elapsed();
    assert!(!manager.contains(id));
    assert!(took < Duration::from_millis(200), "destroy blocked for {took:?}");

    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(child) {
        assert!(Instant::now() < deadline, "child {child} outlived the close");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// `sh` has no job control: the job shares the shell's process group.
#[tokio::test]
async fn destroy_returns_at_once_and_kills_hup_ignoring_child_sh() {
    close_with_hup_ignoring_child("/bin/sh").await;
}

/// `zsh` puts the job in its own foreground process group, which the reaper
/// reads back from the PTY master.
#[tokio::test]
async fn destroy_returns_at_once_and_kills_hup_ignoring_child_zsh() {
    close_with_hup_ignoring_child("/bin/zsh").await;
}
