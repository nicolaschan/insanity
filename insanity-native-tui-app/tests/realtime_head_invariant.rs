use insanity_native_tui_app::realtime_buffer::RealTimeBuffer;

#[test]
fn next_item_advances_head_by_at_most_one() {
    let mut buf = RealTimeBuffer::new(3);
    let mut seq: u128 = 0;
    for step in 0..500 {
        match step % 4 {
            0 | 1 => {
                buf.set(seq, seq);
                seq += 1;
            }
            2 => {
                buf.set(seq + 5, seq + 5);
                seq += 6;
            }
            _ => {}
        }
        let h0 = buf.head();
        let _ = buf.next_item();
        let h1 = buf.head();
        assert!(h1 - h0 <= 1, "head jumped {h0}->{h1} at step {step}");
        assert!(buf.len() <= 3, "len exceeds capacity at step {step}");
    }
}

#[test]
fn far_jump_then_drain_advances_one_slot_per_call() {
    let mut buf = RealTimeBuffer::new(3);
    buf.set(0, 0);
    buf.set(10, 10);
    let h0 = buf.head();
    assert_eq!(h0, 8);
    let _ = buf.next_item();
    assert_eq!(buf.head(), h0 + 1);
    let _ = buf.next_item();
    assert_eq!(buf.head(), h0 + 2);
}
