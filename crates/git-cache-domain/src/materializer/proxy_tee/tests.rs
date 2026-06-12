use super::*;

fn pkt(payload: &[u8]) -> Vec<u8> {
    let mut out = format!("{:04x}", payload.len() + 4).into_bytes();
    out.extend_from_slice(payload);
    out
}

fn band(band: u8, payload: &[u8]) -> Vec<u8> {
    let mut data = vec![band];
    data.extend_from_slice(payload);
    pkt(&data)
}

fn body(lines: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    for line in lines {
        out.extend_from_slice(&pkt(line.as_bytes()));
    }
    out.extend_from_slice(b"0000");
    out.extend_from_slice(&pkt(b"done\n"));
    out
}

const WANT: &str = "want 49d0190b13b77a856ffd34a8b042e8c4b69e1c84";

#[test]
fn plan_accepts_full_closure_request() {
    let plan = plan_upload_pack_tee(&body(&[&format!(
        "{WANT} multi_ack side-band-64k thin-pack ofs-delta agent=git/2.54.0\n"
    )]))
    .expect("eligible");
    assert!(plan.sideband);
    assert!(!plan.blobless);
}

#[test]
fn plan_detects_blobless_and_no_sideband() {
    let plan = plan_upload_pack_tee(&body(&[
        &format!("{WANT} multi_ack thin-pack agent=git/2.54.0\n"),
        "filter blob:none\n",
    ]))
    .expect("eligible");
    assert!(!plan.sideband);
    assert!(plan.blobless);
}

#[test]
fn plan_rejects_haves_shallow_deepen_and_v2() {
    let caps = format!("{WANT} side-band-64k\n");
    assert!(plan_upload_pack_tee(&body(&[
        &caps,
        "have 1111111111111111111111111111111111111111\n"
    ]))
    .is_none());
    assert!(plan_upload_pack_tee(&body(&[&caps, "deepen 1"])).is_none());
    assert!(plan_upload_pack_tee(&body(&[&caps, "deepen-since 12345"])).is_none());
    assert!(plan_upload_pack_tee(&body(&[
        &caps,
        "shallow 1111111111111111111111111111111111111111"
    ]))
    .is_none());
    assert!(plan_upload_pack_tee(&body(&["command=fetch", &caps])).is_none());
    assert!(plan_upload_pack_tee(b"0000").is_none());
}

#[test]
fn demux_extracts_sideband_pack() {
    let mut response = pkt(b"NAK\n");
    response.extend_from_slice(&band(2, b"counting objects\r"));
    response.extend_from_slice(&band(1, b"PACKdata1"));
    response.extend_from_slice(&band(1, b"data2"));
    response.extend_from_slice(b"0000");

    let mut demux = PackDemux::new(true);
    let mut sink = Vec::new();
    // Feed in awkward split points to exercise buffering.
    for chunk in response.chunks(3) {
        demux.feed(chunk, &mut sink).unwrap();
    }
    assert_eq!(sink, b"PACKdata1data2");
    assert!(demux.pack_complete());
    assert_eq!(demux.pack_bytes(), 14);
}

#[test]
fn demux_extracts_raw_pack_without_sideband() {
    let mut response = pkt(b"NAK\n");
    response.extend_from_slice(b"PACKrawbytes");

    let mut demux = PackDemux::new(false);
    let mut sink = Vec::new();
    demux.feed(&response, &mut sink).unwrap();
    demux.feed(b"more", &mut sink).unwrap();
    assert_eq!(sink, b"PACKrawbytesmore");
    assert!(demux.pack_complete());
}

#[test]
fn demux_fails_on_sideband_error_and_err_line() {
    let mut response = pkt(b"NAK\n");
    response.extend_from_slice(&band(3, b"fatal: boom"));
    let mut demux = PackDemux::new(true);
    let mut sink = Vec::new();
    assert!(demux.feed(&response, &mut sink).is_err());
    assert!(!demux.pack_complete());

    let mut demux = PackDemux::new(true);
    assert!(demux.feed(&pkt(b"ERR upstream"), &mut sink).is_err());
}

#[test]
fn demux_without_pack_bytes_is_incomplete() {
    let mut demux = PackDemux::new(true);
    let mut sink = Vec::new();
    demux.feed(&pkt(b"NAK\n"), &mut sink).unwrap();
    assert!(!demux.pack_complete());
}
