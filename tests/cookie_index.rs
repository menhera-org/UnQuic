use unquic::{UnquicUuid, uuid::CookieIndex};

#[test]
fn both_lookup_paths_preserve_indices_and_reject_duplicates() {
    for count in [0, 1, 4, 15, 16, 17, 64, 1024] {
        let cookies: Vec<_> = (0..count)
            .rev()
            .map(|n: u128| UnquicUuid::from_bytes(n.to_be_bytes()))
            .collect();
        let index = CookieIndex::new(cookies.iter().copied()).unwrap();
        for (at, &cookie) in cookies.iter().enumerate() {
            assert_eq!(index.find(cookie), Some(at));
        }
        assert_eq!(
            index.find(UnquicUuid::from_bytes(u128::MAX.to_be_bytes())),
            None
        );
        if let Some(&cookie) = cookies.first() {
            let mut duplicate = cookies.clone();
            duplicate.push(cookie);
            assert!(CookieIndex::new(duplicate).is_err());
        }
    }
}
