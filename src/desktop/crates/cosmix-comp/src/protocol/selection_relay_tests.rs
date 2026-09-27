fn relay_devices(h: &mut KeybindingHarness) -> (u32, u32, u32) {
    let (_, human, _) = named_seat_discovery_traffic(h, HUMAN_SEAT_NAME, false);
    let (_, agent, _) = named_seat_discovery_traffic(h, AGENT_SEAT_NAME, false);
    let manager = h.bind_test_global("zwlr_data_control_manager_v1", 2);
    let human_device = h.allocate_object_id();
    let agent_device = h.allocate_object_id();
    send_request(&mut h.client, manager, 1, &words(&[human_device, human]));
    send_request(&mut h.client, manager, 1, &words(&[agent_device, agent]));
    let _ = h.sync();
    (manager, human_device, agent_device)
}

#[cfg(feature = "xwayland")]
#[test]
fn xwayland_teardown_clears_both_seat_offers_only_while_x11_owns_them() {
    for replace_clipboard in [false, true] {
        let mut h = KeybindingHarness::new(false);
        let (manager, human, agent) = relay_devices(&mut h);
        for target in [SelectionTarget::Clipboard, SelectionTarget::Primary] {
            h.server.state.x11_new_selection(target, vec!["text/plain".into()]);
        }
        let events = h.sync();
        for device in [human, agent] {
            assert_ne!(relay_offer(&events, device, false), 0);
            assert_ne!(relay_offer(&events, device, true), 0);
        }
        let replacement = replace_clipboard.then(|| {
            let source = relay_source(&mut h, manager);
            send_request(&mut h.client, agent, 0, &words(&[source]));
            let events = h.sync();
            (source, relay_offer(&events, human, false))
        });
        // The offline XWM fixture supplies ownership callbacks; use the real
        // shared teardown reached by shutdown and failed generations.
        h.server.state.shutdown_xwayland();
        let events = h.sync();
        for device in [human, agent] {
            assert_eq!(relay_offer(&events, device, true), 0);
            if replacement.is_none() {
                assert_eq!(relay_offer(&events, device, false), 0);
            } else {
                assert!(!events.iter().any(|(object, opcode, _)| *object == device && *opcode == 1));
            }
        }
        if let Some((source, offer)) = replacement {
            relay_transfer(&mut h, offer, source, b"Wayland replacement survives");
        }
    }
}

fn relay_source(h: &mut KeybindingHarness, manager: u32) -> u32 {
    let source = h.allocate_object_id();
    send_request(&mut h.client, manager, 0, &words(&[source]));
    send_request(&mut h.client, source, 0, &wire_string_argument("text/plain"));
    source
}

fn relay_offer(events: &WireEvents, device: u32, primary: bool) -> u32 {
    events.iter().rev().find_map(|(object, opcode, body)| {
        (*object == device && *opcode == if primary { 3 } else { 1 }).then(|| word(body, 0))
    }).expect("seat receives a selection update")
}

fn relay_transfer(h: &mut KeybindingHarness, offer: u32, source: u32, bytes: &[u8]) {
    let _ = drain_buffered_events(&mut h.client);
    let (mut receiver, writer) = UnixStream::pair().unwrap();
    receiver.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    send_request_with_fd(&mut h.client, offer, 0, &wire_string_argument("text/plain"), writer.as_fd());
    drop(writer);
    h.dispatch_client();
    h.client.set_nonblocking(false).unwrap();
    h.client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut bytes_in = [0_u8; 8];
    let mut ancillary = [0_usize; 16];
    let mut iov = libc::iovec { iov_base: bytes_in.as_mut_ptr().cast(), iov_len: bytes_in.len() };
    // SAFETY: msghdr points at live, aligned buffers; received SCM_RIGHTS fd
    // is taken into OwnedFd exactly once and closed by RAII.
    let received = unsafe {
        let mut message: libc::msghdr = std::mem::zeroed();
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = ancillary.as_mut_ptr().cast();
        message.msg_controllen = std::mem::size_of_val(&ancillary);
        assert_eq!(libc::recvmsg(h.client.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC | libc::MSG_WAITALL), 8);
        let header = libc::CMSG_FIRSTHDR(&message);
        assert!(!header.is_null());
        assert_eq!((*header).cmsg_type, libc::SCM_RIGHTS);
        std::os::fd::OwnedFd::from_raw_fd(std::ptr::read(libc::CMSG_DATA(header).cast::<libc::c_int>()))
    };
    assert_eq!(word(&bytes_in, 0), source, "the originating source receives the fd");
    let length = (word(&bytes_in, 1) >> 16) as usize;
    let mut body = vec![0; length - 8];
    h.client.read_exact(&mut body).unwrap();
    h.client.set_nonblocking(true).unwrap();
    let mut source_writer = File::from(received);
    source_writer.write_all(bytes).unwrap();
    drop(source_writer);
    let mut copied = vec![0; bytes.len()];
    receiver.read_exact(&mut copied).unwrap();
    assert_eq!(copied, bytes);
}

#[test]
fn clipboard_and_primary_relay_both_directions_without_ping_pong() {
    for primary in [false, true] {
        let mut h = KeybindingHarness::new(false);
        let (manager, human, agent) = relay_devices(&mut h);
        for (from, to) in [(agent, human), (human, agent)] {
            let source = relay_source(&mut h, manager);
            send_request(&mut h.client, from, if primary { 2 } else { 0 }, &words(&[source]));
            let events = h.sync();
            for device in [human, agent] {
                assert_ne!(relay_offer(&events, device, primary), 0, "history on either seat sees the source");
                assert_eq!(events.iter().filter(|(object, opcode, _)|
                    *object == device && *opcode == if primary { 3 } else { 1 }
                ).count(), 1, "one update per seat, no echo");
            }
            relay_transfer(&mut h, relay_offer(&events, to, primary), source, b"shared selection");
        }
    }
}

#[test]
fn selection_relay_replacement_clear_and_source_death_invalidate_mirrors() {
    for primary in [false, true] {
        let mut h = KeybindingHarness::new(false);
        let (manager, human, agent) = relay_devices(&mut h);
        let opcode = if primary { 2 } else { 0 };
        let old = relay_source(&mut h, manager);
        send_request(&mut h.client, agent, opcode, &words(&[old]));
        let events = h.sync();
        let old_offer = relay_offer(&events, human, primary);
        let replacement = relay_source(&mut h, manager);
        send_request(&mut h.client, human, opcode, &words(&[replacement]));
        let events = h.sync();
        relay_transfer(&mut h, relay_offer(&events, agent, primary), replacement, b"replacement");
        send_request(&mut h.client, old, 1, &[]);
        assert!(!h.sync().iter().any(|(object, opcode, _)| *object == agent && *opcode == if primary { 3 } else { 1 }),
            "destroying replaced source cannot clear current selection");
        let (mut receiver, writer) = UnixStream::pair().unwrap();
        receiver.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        send_request_with_fd(&mut h.client, old_offer, 0, &wire_string_argument("text/plain"), writer.as_fd());
        drop(writer);
        h.dispatch_client();
        assert_eq!(receiver.read(&mut [0_u8; 1]).unwrap(), 0, "stale mirror closes fd instead of reading a newer selection");
        send_request(&mut h.client, replacement, 1, &[]);
        let events = h.sync();
        assert_eq!(relay_offer(&events, human, primary), 0);
        assert_eq!(relay_offer(&events, agent, primary), 0);
        let source = relay_source(&mut h, manager);
        send_request(&mut h.client, agent, opcode, &words(&[source]));
        let _ = h.sync();
        send_request(&mut h.client, agent, opcode, &words(&[0]));
        let events = h.sync();
        assert_eq!(relay_offer(&events, human, primary), 0);
        assert_eq!(relay_offer(&events, agent, primary), 0);
    }
}
