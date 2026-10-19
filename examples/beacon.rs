//! Beacon — a tiny build queue. No cloud required.
//!
//! Run: cargo run --example beacon

use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Debug)]
struct Build {
    name: &'static str,
    steps: usize,
}

impl Build {
    fn run(&self) -> Duration {
        let started = Instant::now();
        for step in 1..=self.steps {
            println!("  [{step}/{}] {}", self.steps, self.name);
        }
        started.elapsed()
    }
}

fn main() {
    let mut queue = VecDeque::from([
        Build {
            name: "compile",
            steps: 3,
        },
        Build {
            name: "verify",
            steps: 2,
        },
        Build {
            name: "ship",
            steps: 1,
        },
    ]);

    println!("BEACON / local build pipeline\n");
    while let Some(build) = queue.pop_front() {
        let elapsed = build.run();
        println!("  done in {elapsed:?}\n");
    }
    println!("All systems green. Keep building.");
}
