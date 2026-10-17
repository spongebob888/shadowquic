use std::{collections::HashMap, sync::Arc, time::Duration};

use tokio::sync::Mutex;

use crate::{Outbound, api::quality::{ConnQuality, SQStatsTrait}, observe::Observer};

mod quality;



pub struct ApiManager {
    sq_outbounds: HashMap<String, Arc<dyn SQStatsTrait>>,
    all_outbounds: HashMap<String, Arc<dyn Outbound>>,
    outbounds_quality: Mutex<HashMap<String,ConnQuality>>,
    observers: HashMap<String, Arc<Observer>>,
}
pub(crate) fn probe_http_outbound(link: url::Url, outbound: Arc<dyn Outbound>, timeout: Duration) -> Option<Duration> {
    todo!()
}