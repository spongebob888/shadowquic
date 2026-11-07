use std::sync::atomic::{AtomicU32, Ordering};

  static REQ_CNT: AtomicU32 = AtomicU32::new(0);
  

pub(crate)  fn fetch_add_req_cnt() -> u32 {
      REQ_CNT.fetch_add(1, Ordering::Relaxed) as u32
}


tokio::task_local! {
    pub(crate) static REQ_ID: u32;
}
