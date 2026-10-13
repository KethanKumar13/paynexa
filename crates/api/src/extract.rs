use axum::{
    Json,
    extract::{FromRequest, FromRequestParts, Path, Request, rejection::JsonRejection},
    http::request::Parts,
};
use serde::de::DeserializeOwned;

use crate::error::AppError;

/// `Json<T>` whose rejections use the API error format (422 `validation_error`).
pub struct ApiJson<T>(pub T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    Json<T>: FromRequest<S, Rejection = JsonRejection>,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(AppError::validation(rejection.body_text())),
        }
    }
}

/// `Path<T>` where an unparsable id is reported as `404 not_found`.
pub struct ApiPath<T>(pub T);

impl<S, T> FromRequestParts<S> for ApiPath<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Path::<T>::from_request_parts(parts, state)
            .await
            .map(|Path(value)| Self(value))
            .map_err(|_| AppError::not_found("resource"))
    }
}
