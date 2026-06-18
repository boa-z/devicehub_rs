// Replay a raw RTP dump (from `DEVICEHUB_DUMP_RTP`) through the real
// HevcDepacketizer to localize corruption offline.
//
//   cargo run --example replay_rtp -- /tmp/rtp.bin /tmp/replay.h265
//
// Record format (repeated): seq u16 BE | ts u32 BE | marker u8 | len u32 BE | payload
//
// Reports input access units (distinct RTP timestamps) vs output frames
// (VCL NALs) the depacketizer emitted — a shortfall means a dropped frame.

use std::io::Write;

use idevice::core_device::HevcDepacketizer;

fn main() {
    let mut args = std::env::args().skip(1);
    let in_path = args.next().expect("usage: replay_rtp <rtp_dump> [out.h265]");
    let out_path = args.next();

    let data = std::fs::read(&in_path).expect("read rtp dump");
    let mut dep = HevcDepacketizer::new();
    let mut out = Vec::new();

    let mut off = 0usize;
    let mut input_aus = 0u64; // distinct RTP timestamps seen
    let mut last_ts: Option<u32> = None;
    let mut markers = 0u64;
    let mut last_seq: Option<u16> = None;
    let mut seq_gaps = 0u64;
    let mut seq_backwards = 0u64;
    let mut pkts = 0u64;

    while off + 11 <= data.len() {
        let seq = u16::from_be_bytes([data[off], data[off + 1]]);
        let ts = u32::from_be_bytes([data[off + 2], data[off + 3], data[off + 4], data[off + 5]]);
        let marker = data[off + 6] != 0;
        let len = u32::from_be_bytes([data[off + 7], data[off + 8], data[off + 9], data[off + 10]])
            as usize;
        off += 11;
        if off + len > data.len() {
            eprintln!("truncated record at packet {pkts}");
            break;
        }
        let payload = &data[off..off + len];
        off += len;
        pkts += 1;

        if last_ts != Some(ts) {
            input_aus += 1;
            last_ts = Some(ts);
        }
        if marker {
            markers += 1;
        }
        if let Some(prev) = last_seq {
            let d = seq.wrapping_sub(prev);
            if d == 0 || d >= 0x8000 {
                seq_backwards += 1;
            } else if d > 1 {
                seq_gaps += 1;
                eprintln!("seq jump +{d} at pkt {pkts} (prev={prev} -> {seq})");
            }
        }
        last_seq = Some(seq);

        dep.push(seq, ts, payload);
        out.extend_from_slice(&dep.take_output());
    }

    // Count VCL NALs (output frames) by scanning Annex-B start codes.
    let mut out_frames = 0u64;
    let mut auds = 0u64;
    let mut i = 0usize;
    while i + 4 < out.len() {
        let sc4 = out[i] == 0 && out[i + 1] == 0 && out[i + 2] == 0 && out[i + 3] == 1;
        let sc3 = out[i] == 0 && out[i + 1] == 0 && out[i + 2] == 1;
        if sc4 || sc3 {
            let h = i + if sc4 { 4 } else { 3 };
            if h < out.len() {
                let t = (out[h] >> 1) & 0x3f;
                if t <= 31 {
                    out_frames += 1;
                } else if t == 35 {
                    auds += 1;
                }
            }
            i = h;
        } else {
            i += 1;
        }
    }

    println!("packets:            {pkts}");
    println!("markers set:        {markers}");
    println!("seq gaps (loss?):   {seq_gaps}");
    println!("seq backwards/dup:  {seq_backwards}");
    println!("input access units: {input_aus}  (distinct RTP timestamps)");
    println!("output VCL frames:  {out_frames}");
    println!("output AUDs:        {auds}");
    if out_frames < input_aus {
        println!(
            "==> DROPPED {} frame(s): depacketizer emitted fewer pictures than arrived.",
            input_aus - out_frames
        );
    }

    if let Some(p) = out_path {
        std::fs::File::create(&p)
            .and_then(|mut f| f.write_all(&out))
            .expect("write out");
        println!("wrote reconstructed stream to {p} ({} bytes)", out.len());
    }
}
