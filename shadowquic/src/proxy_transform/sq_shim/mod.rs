use crate::{TcpSession, error::SError, msgs::squic::SQReq};

struct SqShimServer {}

impl SqShimServer {
    pub async fn transform_tcp_session(&self, session: TcpSession) -> Result<TcpSession, SError> {
        SQReq::decode(session.stream).await?;

        todo!()
    }
}
