use axum::{Json, extract::State, http::HeaderMap};
use chrono::Utc;
use serde_json::{Value, json};

use crate::{
    AppState,
    error::AppError,
    models::Invoice,
    stellar::{find_payment_for_invoice, invoice_is_expired},
};

pub async fn reconcile(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    authorize_cron(&state, &headers)?;
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
            client
                .execute(
                    "UPDATE invoices SET status = 'expired', updated_at = NOW() WHERE id = $1 AND status = 'pending'",
                    &[&invoice.id],
                )
                .await?;
            results.push(json!({ "publicId": invoice.public_id, "action": "expired" }));
            continue;
        }

        match find_payment_for_invoice(&state.config, &invoice).await? {
            Some(payment) => {
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
                transaction
                    .execute(
                        "INSERT INTO payouts (invoice_id, merchant_id, destination_public_key, amount_cents, asset_code, asset_issuer)
                         SELECT id, merchant_id, (SELECT settlement_public_key FROM merchants WHERE merchants.id = invoices.merchant_id),
                                net_amount_cents, asset_code, asset_issuer
                         FROM invoices WHERE id = $1
                         ON CONFLICT (invoice_id) DO NOTHING",
                        &[&invoice.id],
                    )
                    .await?;
                transaction.commit().await?;
                results.push(json!({
                    "publicId": invoice.public_id,
                    "action": "paid",
                    "txHash": payment.hash,
                    "memo": payment.memo
                }));
            }
            None => {
                results.push(json!({ "publicId": invoice.public_id, "action": "pending" }));
            }
        }
    }

    Ok(Json(json!({
        "scanned": results.len(),
        "results": results
    })))
}

pub async fn settle(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    authorize_cron(&state, &headers)?;
    Err(AppError::not_implemented(
        "Rust settlement execution is not implemented yet. Port the Stellar transaction signing/submission path before claiming payout parity.",
    ))
}

fn authorize_cron(state: &AppState, headers: &HeaderMap) -> Result<(), AppError> {
    let token = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if token == Some(state.config.cron_secret.as_str()) {
        Ok(())
    } else {
        Err(AppError::unauthorized("Unauthorized"))
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue};

    use super::authorize_cron;
    use crate::{AppState, config::Config};

    fn state() -> AppState {
        AppState {
            config: Config {
                bind_addr: "127.0.0.1:8080".parse().unwrap(),
                app_url: "http://localhost:3000".to_string(),
                public_app_url: "http://localhost:3000".to_string(),
                database_url: "postgres://postgres:postgres@localhost:5432/astropay".to_string(),
                pgssl: "disable".to_string(),
                session_secret: "secret".to_string(),
                horizon_url: "https://horizon-testnet.stellar.org".to_string(),
                network_passphrase: "Test SDF Network ; September 2015".to_string(),
                stellar_network: "TESTNET".to_string(),
                asset_code: "USDC".to_string(),
                asset_issuer: "ISSUER".to_string(),
                platform_treasury_public_key: "TREASURY".to_string(),
                platform_treasury_secret_key: None,
                platform_fee_bps: 100,
                invoice_expiry_hours: 24,
                cron_secret: "cron_secret".to_string(),
                secure_cookies: false,
            },
            pool: panic_pool(),
        }
    }

    fn panic_pool() -> deadpool_postgres::Pool {
        panic!("pool should not be used in this test")
    }

    #[test]
    fn authorizes_valid_bearer_token() {
        let state = state();
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer cron_secret"),
        );
        assert!(authorize_cron(&state, &headers).is_ok());
    }

    #[test]
    fn rejects_missing_bearer_token() {
        let state = state();
        let headers = HeaderMap::new();
        assert!(authorize_cron(&state, &headers).is_err());
    }
}
