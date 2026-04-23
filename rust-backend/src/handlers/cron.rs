use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, header},
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_postgres::types::Json as PgJson;
use tracing::warn;

use crate::{
    AppState,
    auth::authorize_cron_request,
    error::AppError,
    models::Invoice,
    stellar::{find_payment_for_invoice, invoice_is_expired, is_valid_account_public_key},
};

#[derive(Debug, Deserialize)]
pub struct ReplayRequest {
    #[serde(rename = "publicId")]
    pub public_id: String,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Deserialize, Default)]
pub struct DryRunParams {
    #[serde(default)]
    pub dry_run: bool,
}

pub async fn reconcile(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<DryRunParams>,
) -> Result<Json<Value>, AppError> {
    authorize_cron_request(&state.config.cron_secret, &headers)?;
    let dry_run = params.dry_run;
    let mut client = state.pool.get().await?;
    let rows = client
        .query(
            "SELECT * FROM invoices WHERE status = 'pending' ORDER BY created_at ASC LIMIT 100",
            &[],
        )
        .await?;
    let invoices = rows.iter().map(Invoice::from_row).collect::<Vec<_>>();
    let mut results = Vec::with_capacity(invoices.len());

    for invoice in invoices {
        if invoice_is_expired(&invoice, Utc::now()) {
            if !dry_run {
                client
                    .execute(
                        "UPDATE invoices SET status = 'expired', updated_at = NOW() WHERE id = $1 AND status = 'pending'",
                        &[&invoice.id],
                    )
                    .await?;
            }
            results.push(json!({ "publicId": invoice.public_id, "action": "expired" }));
            continue;
        }

        match find_payment_for_invoice(&state.config, &invoice).await? {
            Some(payment) => {
                if dry_run {
                    results.push(json!({
                        "publicId": invoice.public_id,
                        "action": "paid",
                        "txHash": payment.hash,
                        "memo": payment.memo,
                        "payoutQueued": null,
                        "payoutSkipReason": null
                    }));
                } else {
                    let transaction = client.transaction().await?;
                    transaction
                        .execute(
                            "UPDATE invoices
                             SET status = 'paid', paid_at = NOW(), transaction_hash = $2, updated_at = NOW()
                             WHERE id = $1 AND status = 'pending'",
                            &[&invoice.id, &payment.hash],
                        )
                        .await?;
                    transaction
                        .execute(
                            "INSERT INTO payment_events (invoice_id, event_type, payload) VALUES ($1, $2, $3)",
                            &[&invoice.id, &"payment_detected", &payment.payment],
                        )
                        .await?;
                    let settlement_row = transaction
                        .query_opt(
                            "SELECT m.settlement_public_key
                             FROM merchants m
                             INNER JOIN invoices i ON i.merchant_id = m.id
                             WHERE i.id = $1",
                            &[&invoice.id],
                        )
                        .await?;
                    let settlement_key: Option<String> = settlement_row.map(|row| row.get(0));
                    let settlement_key = settlement_key.unwrap_or_default();
                    let (payout_queued, payout_skip_reason) =
                        if !is_valid_account_public_key(&settlement_key) {
                            transaction
                                .execute(
                                    "INSERT INTO payment_events (invoice_id, event_type, payload) VALUES ($1, $2, $3)",
                                    &[
                                        &invoice.id,
                                        &"payout_skipped_invalid_destination",
                                        &json!({ "reason": "invalid_settlement_public_key" }),
                                    ],
                                )
                                .await?;
                            (false, Some("invalid_settlement_public_key"))
                        } else {
                            let inserted = transaction
                                .execute(
                                    "INSERT INTO payouts (invoice_id, merchant_id, destination_public_key, amount_cents, asset_code, asset_issuer)
                                     SELECT id, merchant_id, (SELECT settlement_public_key FROM merchants WHERE merchants.id = invoices.merchant_id),
                                            net_amount_cents, asset_code, asset_issuer
                                     FROM invoices WHERE id = $1
                                     ON CONFLICT (invoice_id) DO NOTHING",
                                    &[&invoice.id],
                                )
                                .await?;
                            if inserted > 0 {
                                (true, None)
                            } else {
                                (false, Some("payout_already_queued"))
                            }
                        };
                    transaction.commit().await?;
                    results.push(json!({
                        "publicId": invoice.public_id,
                        "action": "paid",
                        "txHash": payment.hash,
                        "memo": payment.memo,
                        "payoutQueued": payout_queued,
                        "payoutSkipReason": payout_skip_reason
                    }));
                }
            }
            None => {
                results.push(json!({ "publicId": invoice.public_id, "action": "pending" }));
            }
        }
    }

    let body = json!({
        "dryRun": dry_run,
        "scanned": results.len(),
        "results": results
    });
    if !dry_run {
        if let Err(e) = client
            .execute(
                "INSERT INTO cron_runs (job_type, started_at, finished_at, success, metadata, error_detail)
                 VALUES ('reconcile', NOW(), NOW(), true, $1, NULL)",
                &[&PgJson(&body)],
            )
            .await
        {
            warn!(error = %e, "cron_runs audit insert failed for reconcile");
        }
    }

    Ok(Json(body))
}

pub async fn settle(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<DryRunParams>,
) -> Result<Json<Value>, AppError> {
    authorize_cron_request(&state.config.cron_secret, &headers)?;
    if params.dry_run {
        return Ok(Json(json!({
            "dryRun": true,
            "processed": 0,
            "results": [],
            "note": "Rust settlement execution is not implemented yet; dry-run returns empty results."
        })));
    }
    Err(AppError::not_implemented(
        "Rust settlement execution is not implemented yet. Port the Stellar transaction signing/submission path before claiming payout parity.",
    ))
}

pub async fn replay_invoice(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ReplayRequest>,
) -> Result<Json<Value>, AppError> {
    authorize_cron_request(&state.config.cron_secret, &headers)?;

    if body.public_id.trim().is_empty() {
        return Err(AppError::bad_request("publicId is required"));
    }

    let mut client = state.pool.get().await?;
    let row = client
        .query_opt(
            "SELECT * FROM invoices WHERE public_id = $1",
            &[&body.public_id],
        )
        .await?
        .ok_or_else(|| AppError::not_found(format!("Invoice '{}' not found", body.public_id)))?;

    let invoice = Invoice::from_row(&row);
    let dry_run = body.dry_run;

    if invoice.status != "pending" {
        return Ok(Json(json!({
            "dryRun": dry_run,
            "publicId": invoice.public_id,
            "action": "skipped",
            "reason": format!("invoice status is '{}', only 'pending' invoices can be replayed", invoice.status)
        })));
    }

    if invoice_is_expired(&invoice, Utc::now()) {
        if !dry_run {
            client
                .execute(
                    "UPDATE invoices SET status = 'expired', updated_at = NOW() WHERE id = $1 AND status = 'pending'",
                    &[&invoice.id],
                )
                .await?;
        }
        return Ok(Json(json!({
            "dryRun": dry_run,
            "publicId": invoice.public_id,
            "action": "expired"
        })));
    }

    match find_payment_for_invoice(&state.config, &invoice).await? {
        None => Ok(Json(json!({
            "dryRun": dry_run,
            "publicId": invoice.public_id,
            "action": "pending"
        }))),
        Some(payment) => {
            if dry_run {
                return Ok(Json(json!({
                    "dryRun": true,
                    "publicId": invoice.public_id,
                    "action": "paid",
                    "txHash": payment.hash,
                    "memo": payment.memo,
                    "payoutQueued": null,
                    "payoutSkipReason": null
                })));
            }

            let transaction = client.transaction().await?;
            transaction
                .execute(
                    "UPDATE invoices
                     SET status = 'paid', paid_at = NOW(), transaction_hash = $2, updated_at = NOW()
                     WHERE id = $1 AND status = 'pending'",
                    &[&invoice.id, &payment.hash],
                )
                .await?;
            transaction
                .execute(
                    "INSERT INTO payment_events (invoice_id, event_type, payload) VALUES ($1, $2, $3)",
                    &[&invoice.id, &"payment_detected", &payment.payment],
                )
                .await?;
            let settlement_row = transaction
                .query_opt(
                    "SELECT m.settlement_public_key
                     FROM merchants m
                     INNER JOIN invoices i ON i.merchant_id = m.id
                     WHERE i.id = $1",
                    &[&invoice.id],
                )
                .await?;
            let settlement_key = settlement_row
                .map(|r| r.get::<_, String>(0))
                .unwrap_or_default();
            let (payout_queued, payout_skip_reason) =
                if !is_valid_account_public_key(&settlement_key) {
                    transaction
                        .execute(
                            "INSERT INTO payment_events (invoice_id, event_type, payload) VALUES ($1, $2, $3)",
                            &[
                                &invoice.id,
                                &"payout_skipped_invalid_destination",
                                &json!({ "reason": "invalid_settlement_public_key" }),
                            ],
                        )
                        .await?;
                    (false, Some("invalid_settlement_public_key"))
                } else {
                    let inserted = transaction
                        .execute(
                            "INSERT INTO payouts (invoice_id, merchant_id, destination_public_key, amount_cents, asset_code, asset_issuer)
                             SELECT id, merchant_id, (SELECT settlement_public_key FROM merchants WHERE merchants.id = invoices.merchant_id),
                                    net_amount_cents, asset_code, asset_issuer
                             FROM invoices WHERE id = $1
                             ON CONFLICT (invoice_id) DO NOTHING",
                            &[&invoice.id],
                        )
                        .await?;
                    if inserted > 0 { (true, None) } else { (false, Some("payout_already_queued")) }
                };
            transaction.commit().await?;
            Ok(Json(json!({
                "dryRun": false,
                "publicId": invoice.public_id,
                "action": "paid",
                "txHash": payment.hash,
                "memo": payment.memo,
                "payoutQueued": payout_queued,
                "payoutSkipReason": payout_skip_reason
            })))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DryRunParams;

    #[test]
    fn dry_run_defaults_to_false() {
        let p: DryRunParams = serde_urlencoded::from_str("").unwrap();
        assert!(!p.dry_run);
    }

    #[test]
    fn dry_run_true_parses() {
        let p: DryRunParams = serde_urlencoded::from_str("dry_run=true").unwrap();
        assert!(p.dry_run);
    }

    #[test]
    fn dry_run_false_parses() {
        let p: DryRunParams = serde_urlencoded::from_str("dry_run=false").unwrap();
        assert!(!p.dry_run);
    }
}

#[cfg(test)]
mod replay_tests {
    use super::ReplayRequest;

    #[test]
    fn replay_request_dry_run_defaults_false() {
        let r: ReplayRequest = serde_json::from_str(r#"{"publicId":"inv_abc"}"#).unwrap();
        assert_eq!(r.public_id, "inv_abc");
        assert!(!r.dry_run);
    }

    #[test]
    fn replay_request_dry_run_true() {
        let r: ReplayRequest =
            serde_json::from_str(r#"{"publicId":"inv_abc","dry_run":true}"#).unwrap();
        assert!(r.dry_run);
    }

    #[test]
    fn replay_request_missing_public_id_fails() {
        assert!(serde_json::from_str::<ReplayRequest>(r#"{}"#).is_err());
    }
}
