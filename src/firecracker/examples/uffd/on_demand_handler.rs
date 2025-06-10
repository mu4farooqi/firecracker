// Copyright 2022 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Provides functionality for a userspace page fault handler
//! which loads the whole region from the backing memory file
//! when a page fault occurs.

mod uffd_utils;

use std::fs::File;
use std::os::unix::net::UnixListener;

use uffd_utils::{Runtime, UffdHandler};
use userfaultfd::Event;

fn main() {
    let mut args = std::env::args();
    let uffd_sock_path = args.nth(1).expect("No socket path given");
    let mem_file_path = args.next().expect("No memory file given");

    let file = File::open(mem_file_path).expect("Cannot open memfile");

    // Get Uffd from UDS. We'll use the uffd to handle PFs for Firecracker.
    let listener = UnixListener::bind(uffd_sock_path).expect("Cannot bind to socket path");
    let (stream, _) = listener.accept().expect("Cannot listen on UDS socket");

    let mut runtime = Runtime::new(stream, file);
    runtime.install_panic_hook();
    runtime.run(|uffd_handler: &mut UffdHandler| {
        // FIXED: This implementation properly handles the complexity by ensuring that
        // `remove` events are always processed before `pagefault` events in each batch.
        // This avoids the race condition where pagefaults might be processed before
        // their corresponding remove events, ensuring correct zero-page vs file-backed behavior.

        let mut deferred_pagefaults: Vec<*mut u8> = Vec::new();

        loop {
            // Phase 1: Collect all available events
            let mut events = Vec::new();

            // First process any deferred pagefaults from previous iteration
            for addr in deferred_pagefaults.drain(..) {
                events.push(Event::Pagefault {
                    addr,
                    flags: userfaultfd::ReadWrite::Read
                });
            }

            // Then read all new events from UFFD
            while let Some(event) = uffd_handler.read_event().expect("Failed to read uffd_msg") {
                events.push(event);
            }

            // If no events to process, we're done
            if events.is_empty() {
                break;
            }

            // Phase 2: Process all Remove events first
            // This ensures proper ordering: removes are always handled before pagefaults
            for event in &events {
                if let Event::Remove { start, end } = *event {
                    uffd_handler.mark_range_removed(start as u64, end as u64);
                }
            }

            // Phase 3: Process Pagefault events
            // After all removes are processed, handle pagefaults in arrival order
            let mut new_deferred = Vec::new();

            for event in events {
                if let Event::Pagefault { addr, .. } = event {
                    // serve_pf returns false if it encounters EAGAIN (due to pending remove events)
                    if !uffd_handler.serve_pf(addr.cast(), uffd_handler.page_size) {
                        // Defer this pagefault to be retried in the next iteration
                        new_deferred.push(addr);
                    }
                }
                // Other event types are ignored as per original logic
            }

            deferred_pagefaults = new_deferred;

            // Continue looping if we have deferred pagefaults
            // This handles the case where remove events arrive after we've read the initial batch
        }
    });
}
