use std::collections::HashMap;
use std::task::{Context, Poll};

use vm::{CallReturn, HostAsyncBridge, HostFuture, HostFutureOutput, HostOpId, VmError, VmResult};

/// Embedding-owned async bridge. One instance belongs to one VM.
///
/// Submitted futures are polled by the VM's waiting-host-op path. Cancelling
/// an operation drops the future, which is the Core client's cancellation
/// signal. This bridge is not shared across threads or VMs.
#[derive(Default)]
pub struct EmbeddingBridge {
    submitted: HashMap<HostOpId, HostFuture>,
}

impl HostAsyncBridge for EmbeddingBridge {
    fn submit_op(&mut self, op_id: HostOpId, future: HostFuture) -> VmResult<()> {
        self.submitted.insert(op_id, future);
        Ok(())
    }

    fn poll_op(&mut self, op_id: HostOpId, cx: &mut Context<'_>) -> Poll<VmResult<CallReturn>> {
        match self.poll_submitted_op(op_id, cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(HostFutureOutput::Return(value))) => Poll::Ready(Ok(value)),
            Poll::Ready(Ok(HostFutureOutput::VmCompletion(_))) => {
                Poll::Ready(Err(VmError::HostError(format!(
                    "host operation {op_id} completed with a VM callback on the external poll path"
                ))))
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
        }
    }

    fn poll_submitted_op(
        &mut self,
        op_id: HostOpId,
        cx: &mut Context<'_>,
    ) -> Poll<VmResult<HostFutureOutput>> {
        let poll = match self.submitted.get_mut(&op_id) {
            Some(future) => future.as_mut().poll(cx),
            None => {
                return Poll::Ready(Err(VmError::HostError(format!(
                    "unknown submitted host operation {op_id}"
                ))));
            }
        };
        if poll.is_ready() {
            self.submitted.remove(&op_id);
        }
        poll
    }

    fn cancel_op(&mut self, op_id: HostOpId) {
        self.submitted.remove(&op_id);
    }
}
