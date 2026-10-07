//! Errors people see. Messages never include backend details.

use std::fmt;

use axum::Json;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// Why a wallet action did not happen, worded for the person who asked.
#[derive(Debug)]
pub(crate) enum WalletError {
    Invalid(String),
    LimitExceeded(String),
    InsufficientBalance,
    Frozen,
    NotFound,
    AlreadyPaid,
    /// The account holds as many unpaid invoices as it may.
    TooManyInvoices,
    Unavailable,
    Internal(anyhow::Error),
}

impl WalletError {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }

    pub(crate) fn limit(message: impl Into<String>) -> Self {
        Self::LimitExceeded(message.into())
    }
}

impl fmt::Display for WalletError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) | Self::LimitExceeded(message) => f.write_str(message),
            Self::InsufficientBalance => f.write_str("Not enough sats in this wallet."),
            Self::Frozen => f.write_str("This account is frozen. Contact the operator."),
            Self::NotFound => f.write_str("Not found."),
            Self::AlreadyPaid => f.write_str("This invoice is already paid or being paid."),
            Self::TooManyInvoices => {
                f.write_str("Too many unpaid invoices. Wait for one to be paid or to expire, then try again.")
            }
            Self::Unavailable => f.write_str("The Lightning node is unavailable. Try again shortly."),
            Self::Internal(_) => f.write_str("Something went wrong. Try again."),
        }
    }
}

impl From<sqlx::Error> for WalletError {
    fn from(error: sqlx::Error) -> Self {
        if error
            .as_database_error()
            .is_some_and(|database| database.message().contains("insufficient balance"))
        {
            return Self::InsufficientBalance;
        }
        Self::Internal(error.into())
    }
}

/// A public LNURL error. Static reasons keep backend details private.
#[derive(Debug, Serialize)]
pub(crate) struct LnurlError {
    status: &'static str,
    reason: String,
}

impl LnurlError {
    pub(crate) fn new(reason: impl Into<String>) -> Self {
        Self {
            status: "ERROR",
            reason: reason.into(),
        }
    }
}

impl IntoResponse for LnurlError {
    fn into_response(self) -> Response {
        Json(self).into_response()
    }
}

impl std::error::Error for WalletError {}
