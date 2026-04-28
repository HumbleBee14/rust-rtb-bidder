use axum::{body::Body, extract::MatchedPath, http::Request, response::Response};
use futures_util::future::BoxFuture;
use metrics::{counter, histogram};
use std::{
    task::{Context, Poll},
    time::Instant,
};
use tower::{Layer, Service};

#[derive(Clone)]
pub struct MetricsLayer;

impl<S> Layer<S> for MetricsLayer {
    type Service = MetricsMiddleware<S>;

    fn layer(&self, inner: S) -> Self::Service {
        MetricsMiddleware { inner }
    }
}

#[derive(Clone)]
pub struct MetricsMiddleware<S> {
    inner: S,
}

impl<S> Service<Request<Body>> for MetricsMiddleware<S>
where
    S: Service<Request<Body>, Response = Response> + Send + Clone + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let route = req
            .extensions()
            .get::<MatchedPath>()
            .map(|p| p.as_str().to_owned())
            .unwrap_or_else(|| "unknown".to_owned());
        let method = req.method().to_string();
        let start = Instant::now();
        let mut inner = self.inner.clone();

        Box::pin(async move {
            let response = inner.call(req).await?;
            let status = response.status().as_u16().to_string();
            let elapsed = start.elapsed().as_secs_f64();

            counter!("bidder.http.requests_total", "method" => method.clone(), "route" => route.clone(), "status" => status.clone()).increment(1);
            histogram!("bidder.http.request_duration_seconds", "method" => method, "route" => route, "status" => status).record(elapsed);

            Ok(response)
        })
    }
}
