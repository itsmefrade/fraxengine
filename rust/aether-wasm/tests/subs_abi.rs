//! Native calls of the `aether_subs_*` exports (same ABI the browser uses).
use aether_wasm::subs::*;
use aether_wasm::{aether_result_len, aether_result_ptr};

fn result() -> Vec<u8> {
    unsafe { std::slice::from_raw_parts(aether_result_ptr(), aether_result_len()).to_vec() }
}

#[test]
fn srt_packets_and_dedupe() {
    assert_eq!(unsafe { aether_subs_open(2, std::ptr::null(), 0) }, 0);
    assert!(String::from_utf8(result()).unwrap().contains("\"playResX\":384"));
    let p = b"<i>Hi</i>";
    assert_eq!(unsafe { aether_subs_push(1_000_000, 2_000_000, p.as_ptr(), p.len()) }, 1);
    let j = String::from_utf8(result()).unwrap();
    assert!(j.contains("\"t\":\"Hi\"") && j.contains("\"i\":1") && j.contains("\"e\":3000000"), "{j}");
    assert_eq!(unsafe { aether_subs_push(1_000_000, 2_000_000, p.as_ptr(), p.len()) }, 0);
    aether_subs_close();
    assert_eq!(unsafe { aether_subs_push(0, 0, p.as_ptr(), p.len()) }, -1);
}

#[test]
fn pgs_sup_file_and_blocks() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../aether-subs/tests/fixtures/pocawe.sup");
    let Ok(sup) = std::fs::read(path) else {
        eprintln!("skip: {path} absent (git-ignored fixture derived from lab media)");
        return;
    };
    assert_eq!(unsafe { aether_subs_load_file(0, sup.as_ptr(), sup.len()) }, 21);
    let r = result();
    assert_eq!(u32::from_le_bytes(r[0..4].try_into().unwrap()), 21);
    assert_eq!(unsafe { aether_subs_open(0, std::ptr::null(), 0) }, 0);
    let (pts, block) = aether_subs::pgs::sup_to_blocks(&sup).unwrap().remove(0);
    assert_eq!(unsafe { aether_subs_push(pts, 0, block.as_ptr(), block.len()) }, 1);
    let r = result();
    assert_eq!(i64::from_le_bytes(r[4..12].try_into().unwrap()), pts);
    assert_eq!(i64::from_le_bytes(r[12..20].try_into().unwrap()), -1);
    assert_eq!(u32::from_le_bytes(r[32..36].try_into().unwrap()), 1, "one bitmap");
    assert_eq!(unsafe { aether_subs_load_file(9, sup.as_ptr(), 4) }, -2);
}
