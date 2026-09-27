//! Document chat input validation tests.

use crate::*;

#[test]
fn validates_chat_document_id_filters() {
    assert_eq!(
        normalize_chat_document_ids(Some(vec![2, 2, 3]))
            .expect("valid ids")
            .expect("some ids"),
        vec![2, 3]
    );
    assert!(
        normalize_chat_document_ids(Some(vec![0]))
            .expect_err("zero is rejected")
            .status
            == StatusCode::BAD_REQUEST
    );
    assert!(
        normalize_chat_document_ids(Some(vec![1; MAX_CHAT_DOCUMENT_FILTER_IDS + 1]))
            .expect_err("oversized filter is rejected")
            .status
            == StatusCode::BAD_REQUEST
    );
}
