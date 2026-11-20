use super::{ApiError, ApiManager, HttpResponse, decode_tag, json_body, response};
use crate::plugin::database::{ListEdit, RuleError};
use hyper::{Method, Request, StatusCode, body::Incoming};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Rules {
    rules: Vec<String>,
}

#[derive(Serialize)]
struct Lists<T> {
    lists: T,
}

impl ApiManager {
    pub(super) async fn database_request(
        &self,
        request: Request<Incoming>,
    ) -> Result<HttpResponse, ApiError> {
        let tail = request
            .uri()
            .path()
            .strip_prefix("/api/v1/databases/")
            .unwrap();
        let parts: Vec<_> = tail.split('/').collect();
        let (tag, list, rules_endpoint) = match parts.as_slice() {
            [tag, "rules"] if !tag.is_empty() => (*tag, None, true),
            [tag, "lists"] if !tag.is_empty() => (*tag, None, false),
            [tag, "lists", name] if !tag.is_empty() && !name.is_empty() => {
                (*tag, Some(decode_tag(name)?), false)
            }
            [tag, "lists", name, "rules"] if !tag.is_empty() && !name.is_empty() => {
                (*tag, Some(decode_tag(name)?), true)
            }
            _ => {
                return Err(ApiError::new(
                    StatusCode::NOT_FOUND,
                    "not_found",
                    "unknown route",
                ));
            }
        };
        let db = self
            .custom_databases
            .get(&decode_tag(tag)?)
            .cloned()
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::NOT_FOUND,
                    "not_found",
                    "unknown custom database",
                )
            })?;
        let edit = match (request.method(), list.is_some(), rules_endpoint) {
            (&Method::GET, _, false) | (&Method::GET, false, true) => None,
            (&Method::DELETE, true, false) => Some(ListEdit::Delete),
            (&Method::PUT, true, false) | (&Method::POST | &Method::DELETE, true, true) => {
                let method = request.method().clone();
                let input: Rules = serde_json::from_slice(&json_body(request).await?)
                    .map_err(ApiError::invalid)?;
                Some(match method {
                    Method::PUT => ListEdit::Replace(input.rules),
                    Method::POST => ListEdit::Add(input.rules),
                    _ => ListEdit::Remove(input.rules),
                })
            }
            _ => {
                return Err(ApiError::new(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "method_not_allowed",
                    "unsupported method",
                ));
            }
        };
        tokio::task::spawn_blocking(move || -> crate::plugin::database::Result<_> {
            Ok(match (list, edit) {
                (Some(list), Some(edit)) => {
                    db.edit_list(&list, edit)?;
                    response(StatusCode::OK, json!({"list": list.to_ascii_lowercase()}))
                }
                (Some(list), None) => {
                    response(StatusCode::OK, json!({"rules": db.list_rules(&list)?}))
                }
                (None, _) if rules_endpoint => response(
                    StatusCode::OK,
                    Lists {
                        lists: db.all_list_rules()?,
                    },
                ),
                (None, _) => response(
                    StatusCode::OK,
                    Lists {
                        lists: db.list_names()?,
                    },
                ),
            })
        })
        .await
        .map_err(internal)?
        .map_err(|error| match error.downcast_ref::<RuleError>() {
            Some(RuleError::MissingList) => {
                ApiError::new(StatusCode::NOT_FOUND, "not_found", error)
            }
            Some(RuleError::Invalid(_)) => ApiError::invalid(error),
            None => internal(error),
        })
    }
}

fn internal(error: impl std::fmt::Display) -> ApiError {
    tracing::error!(%error, "custom database request failed");
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "database_error",
        "custom database request failed",
    )
}
