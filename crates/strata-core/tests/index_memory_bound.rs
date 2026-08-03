//! Cache stays byte-bounded at 100k entries across 100 pages.

use std::sync::Arc;
use strata_core::index::{IndexKey, IndexPage, IndexVal, SieveCache};

#[test]
fn hundred_thousand_entries_bounded_cache() {
    let mut c = SieveCache::new(512 * 1024);
    // 100 pages × 1000 entries; x is globally unique (page p entry i -> x = p*1000+i).
    for p in 0..100u32 {
        let entries: Vec<_> = (0..1000)
            .map(|i| {
                let x = (p * 1000 + i) as i32;
                (
                    IndexKey { x, z: 0, type_id: 0 },
                    IndexVal {
                        seg_id: p,
                        offset: 16,
                        payload_len: 5,
                        gen: x as u64,
                        comp_id: 1,
                    },
                )
            })
            .collect();
        c.put(p, Arc::new(IndexPage::from_entries(entries)));
    }
    assert!(c.len_bytes() <= 512 * 1024);
    // A page still resident must answer lookups correctly (seg 99 was put last,
    // so nothing evicted it afterwards).
    if let Some(page) = c.get(99) {
        let v = page
            .lookup(&IndexKey {
                x: 99_999,
                z: 0,
                type_id: 0,
            })
            .unwrap();
        assert_eq!(v.gen, 99_999);
    }
}
