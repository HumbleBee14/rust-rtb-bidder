use crate::{
    model::{
        openrtb::{Bid, BidResponse, NoBidReason, SeatBid},
        BidContext, PipelineOutcome,
    },
    pipeline::Stage,
};

/// Serializes ctx.winners into an OpenRTB BidResponse stored on the context.
///
/// If the pipeline is still Pending (no stage set an outcome), falls back to
/// NoBid(NO_ELIGIBLE_BIDS). Called last; outcome is already set by RankingStage.
pub struct ResponseBuildStage;

impl Stage for ResponseBuildStage {
    fn name(&self) -> &'static str {
        "response_build"
    }

    #[allow(clippy::manual_async_fn)]
    fn execute<'a>(
        &'a self,
        ctx: &'a mut BidContext,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send + 'a {
        async move {
            if ctx.outcome == PipelineOutcome::Pending {
                ctx.outcome = PipelineOutcome::NoBid(NoBidReason::NO_ELIGIBLE_BIDS);
            }

            match &ctx.outcome {
                PipelineOutcome::NoBid(reason) => {
                    ctx.bid_response = Some(BidResponse::no_bid(ctx.request.id.clone(), *reason));
                }
                PipelineOutcome::Bid => {
                    let bids: Vec<Bid> = ctx
                        .winners
                        .iter()
                        .map(|w| Bid {
                            id: format!("{}-{}", ctx.request.id, w.imp_id),
                            impid: w.imp_id.clone(),
                            // price is in USD; bid_price_cents / 100
                            price: w.bid_price_cents as f64 / 100.0,
                            adid: Some(w.creative_id.to_string()),
                            cid: Some(w.campaign_id.to_string()),
                            crid: Some(w.creative_id.to_string()),
                            nurl: None,
                            burl: None,
                            lurl: None,
                            adm: None,
                            adomain: None,
                            bundle: None,
                            iurl: None,
                            tactic: None,
                            cat: vec![],
                            attr: vec![],
                            api: None,
                            protocol: None,
                            qagmediarating: None,
                            language: None,
                            dealid: None,
                            w: None,
                            h: None,
                            wratio: None,
                            hratio: None,
                            exp: None,
                            ext: None,
                        })
                        .collect();

                    ctx.bid_response = Some(BidResponse {
                        id: ctx.request.id.clone(),
                        seatbid: vec![SeatBid {
                            bid: bids,
                            seat: None,
                            group: 0,
                            ext: None,
                        }],
                        bidid: None,
                        cur: Some("USD".to_string()),
                        customdata: None,
                        nbr: None,
                        ext: None,
                    });
                }
                PipelineOutcome::Pending => unreachable!(),
            }

            Ok(())
        }
    }
}
