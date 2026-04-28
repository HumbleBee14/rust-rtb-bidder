use crate::model::BidContext;
use std::future::Future;

/// A single processing step in the bid pipeline.
///
/// Each stage receives the full `BidContext`, mutates it in place, and returns
/// `Ok(())` to continue or an error to abort. Early exit (no-bid decision) is
/// expressed by setting `ctx.outcome` — not by returning an error. Errors
/// indicate unexpected failures (e.g. Redis unreachable when there's no fallback).
pub trait Stage: Send + Sync + 'static {
    fn name(&self) -> &'static str;

    fn execute<'a>(
        &'a self,
        ctx: &'a mut BidContext,
    ) -> impl Future<Output = anyhow::Result<()>> + Send + 'a;
}
