use std::collections::HashMap;

use common_enums::enums;
use common_utils::{ext_traits::ValueExt, types::StringMinorUnit};
use error_stack::ResultExt;
use hyperswitch_domain_models::{
    payment_method_data::Card,
    router_data::{ConnectorAuthType, RouterData},
    router_flow_types::refunds::{Execute, RSync},
    router_request_types::ResponseId,
    router_response_types::{PaymentsResponseData, RedirectForm, RefundsResponseData},
    types::{
        PaymentsAuthorizeRouterData, PaymentsCaptureRouterData,
        PaymentsCompleteAuthorizeRouterData, PaymentsSyncRouterData, RefundSyncRouterData,
        RefundsRouterData,
    },
};
use hyperswitch_interfaces::errors;
use masking::{PeekInterface, Secret};
use serde::{Deserialize, Serialize};

use crate::{
    types::{
        PaymentsCancelResponseRouterData, PaymentsCaptureResponseRouterData,
        RefundsResponseRouterData, ResponseRouterData,
    },
    utils::PaymentsAuthorizeRequestData,
};

// ---------------------------------------------------------------------------
// Amount wrapper
// ---------------------------------------------------------------------------

pub struct AtbbankRouterData<T> {
    pub amount: StringMinorUnit,
    pub router_data: T,
}

impl<T> From<(StringMinorUnit, T)> for AtbbankRouterData<T> {
    fn from((amount, router_data): (StringMinorUnit, T)) -> Self {
        Self {
            amount,
            router_data,
        }
    }
}

// ---------------------------------------------------------------------------
// Currency helpers  (ISO 4217 numeric)
// ---------------------------------------------------------------------------

fn currency_to_numeric(currency: enums::Currency) -> Result<String, error_stack::Report<errors::ConnectorError>> {
    match currency {
        enums::Currency::AZN => Ok("944".to_string()),
        enums::Currency::USD => Ok("840".to_string()),
        enums::Currency::EUR => Ok("978".to_string()),
        enums::Currency::GBP => Ok("826".to_string()),
        enums::Currency::TRY => Ok("949".to_string()),
        enums::Currency::RUB => Ok("643".to_string()),
        other => Err(errors::ConnectorError::NotSupported {
            message: format!("Currency {other}"),
            connector: "atbbank",
        }
        .into()),
    }
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

/// ATB Bank uses BodyKey auth: `api_key` = userName, `key1` = password.
pub struct AtbbankAuthType {
    pub(super) user_name: Secret<String>,
    pub(super) password: Secret<String>,
}

impl TryFrom<&ConnectorAuthType> for AtbbankAuthType {
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(auth_type: &ConnectorAuthType) -> Result<Self, Self::Error> {
        match auth_type {
            ConnectorAuthType::BodyKey { api_key, key1 } => Ok(Self {
                user_name: api_key.to_owned(),
                password: key1.to_owned(),
            }),
            _ => Err(errors::ConnectorError::FailedToObtainAuthType.into()),
        }
    }
}

// ---------------------------------------------------------------------------
// Connector metadata (persisted between authorize -> capture/void/refund)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub struct AtbbankMeta {
    /// The `orderId` returned by register.do — needed for all subsequent calls.
    pub order_id: String,
    /// Card data saved during Authorize for H2H flow (used in CompleteAuthorize
    /// where payment_method_data is not available via redirect callback).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card_number: Option<Secret<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card_cvc: Option<Secret<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card_exp_month: Option<Secret<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card_exp_year: Option<Secret<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card_holder: Option<String>,
}

// ---------------------------------------------------------------------------
// Step 1 — register.do  (form-urlencoded)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct AtbbankRegisterRequest {
    #[serde(rename = "userName")]
    pub user_name: Secret<String>,
    pub password: Secret<String>,
    #[serde(rename = "orderNumber")]
    pub order_number: String,
    /// Amount in minor units (qepik / cents).
    pub amount: String,
    /// ISO 4217 numeric currency code as string.
    pub currency: String,
    #[serde(rename = "returnUrl")]
    pub return_url: String,
    #[serde(rename = "failUrl")]
    pub fail_url: String,
    pub language: String,
}

impl TryFrom<&AtbbankRouterData<&PaymentsAuthorizeRouterData>> for AtbbankRegisterRequest {
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(
        item: &AtbbankRouterData<&PaymentsAuthorizeRouterData>,
    ) -> Result<Self, Self::Error> {
        let router_data = item.router_data;
        let auth = AtbbankAuthType::try_from(&router_data.connector_auth_type)?;
        let currency_code = currency_to_numeric(router_data.request.currency)?;
        let return_url = router_data.request.get_router_return_url()?;

        // Use the payment_id as the unique order number for ATB
        let order_number = router_data.connector_request_reference_id.clone();

        let language = router_data
            .request
            .get_optional_language_from_browser_info()
            .unwrap_or_else(|| "en".to_string());

        Ok(Self {
            user_name: auth.user_name,
            password: auth.password,
            order_number,
            amount: item.amount.to_string(),
            currency: currency_code,
            return_url: return_url.clone(),
            fail_url: return_url,
            language,
        })
    }
}

// ---------------------------------------------------------------------------
// Step 1 response — register.do
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AtbbankRegisterResponse {
    /// UUID order identifier assigned by SmartVista.
    pub order_id: Option<String>,
    /// URL to redirect the cardholder for payment (checkout-style).
    pub form_url: Option<String>,
    /// 0 = success, non-zero = error.
    pub error_code: Option<i32>,
    pub error_message: Option<String>,
}

// ---------------------------------------------------------------------------
// Step 2 — paymentorder.do  (form-urlencoded, card data)
// ---------------------------------------------------------------------------

/// Request body for `paymentorder.do` — direct card submission.
/// SmartVista uses dollar-prefixed field names for PAN/CVC.
#[derive(Debug, Serialize)]
pub struct AtbbankPaymentOrderRequest {
    #[serde(rename = "userName")]
    pub user_name: Secret<String>,
    pub password: Secret<String>,
    /// The orderId from register.do (MDORDER in SmartVista docs).
    #[serde(rename = "MDORDER")]
    pub mdorder: String,
    /// Card number (dollar-sign prefix in SmartVista).
    #[serde(rename = "$PAN")]
    pub pan: Secret<String>,
    /// CVV/CVC.
    #[serde(rename = "$CVC")]
    pub cvc: Secret<String>,
    /// 4-digit expiry year.
    #[serde(rename = "YYYY")]
    pub expiry_year: Secret<String>,
    /// 2-digit expiry month.
    #[serde(rename = "MM")]
    pub expiry_month: Secret<String>,
    /// Cardholder name.
    #[serde(rename = "TEXT")]
    pub text: String,
    pub language: String,
}

impl AtbbankPaymentOrderRequest {
    pub fn try_new(
        auth: &AtbbankAuthType,
        order_id: &str,
        card: &Card,
        language: &str,
    ) -> Result<Self, error_stack::Report<errors::ConnectorError>> {
        Ok(Self {
            user_name: auth.user_name.clone(),
            password: auth.password.clone(),
            mdorder: order_id.to_string(),
            pan: Secret::new(card.card_number.get_card_no()),
            cvc: card.card_cvc.clone(),
            expiry_year: card.card_exp_year.clone(),
            expiry_month: card.card_exp_month.clone(),
            text: card
                .card_holder_name
                .as_ref()
                .map(|n| n.peek().to_string())
                .unwrap_or_default(),
            language: language.to_string(),
        })
    }
}

impl TryFrom<&PaymentsCompleteAuthorizeRouterData> for AtbbankPaymentOrderRequest {
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(item: &PaymentsCompleteAuthorizeRouterData) -> Result<Self, Self::Error> {
        let auth = AtbbankAuthType::try_from(&item.connector_auth_type)?;

        // Get orderId + optional card data from connector metadata (saved during register.do step)
        let meta: AtbbankMeta = item
            .request
            .connector_meta
            .clone()
            .ok_or(errors::ConnectorError::MissingRequiredField {
                field_name: "connector_meta (AtbbankMeta)",
            })?
            .parse_value("AtbbankMeta")
            .change_context(errors::ConnectorError::RequestEncodingFailed)?;

        let language = "en".to_string();

        // Try payment_method_data first (standard Hyperswitch flow),
        // then fallback to card data saved in connector_meta (H2H redirect flow).
        if let Some(hyperswitch_domain_models::payment_method_data::PaymentMethodData::Card(c)) =
            item.request.payment_method_data.as_ref()
        {
            Self::try_new(&auth, &meta.order_id, &c, &language)
        } else if let Some(ref pan) = meta.card_number {
            // H2H fallback: card data was saved in connector_metadata during Authorize
            let cvc = meta.card_cvc.ok_or(errors::ConnectorError::MissingRequiredField {
                field_name: "card_cvc in connector_meta",
            })?;
            let exp_year = meta.card_exp_year.ok_or(errors::ConnectorError::MissingRequiredField {
                field_name: "card_exp_year in connector_meta",
            })?;
            let exp_month = meta.card_exp_month.ok_or(errors::ConnectorError::MissingRequiredField {
                field_name: "card_exp_month in connector_meta",
            })?;

            Ok(Self {
                user_name: auth.user_name,
                password: auth.password,
                mdorder: meta.order_id,
                pan: pan.clone(),
                cvc,
                expiry_year: exp_year,
                expiry_month: exp_month,
                text: meta.card_holder.unwrap_or_default(),
                language,
            })
        } else {
            Err(errors::ConnectorError::MissingRequiredField {
                field_name: "payment_method_data (Card) or card data in connector_meta",
            }
            .into())
        }
    }
}

// ---------------------------------------------------------------------------
// Step 2 response — paymentorder.do
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AtbbankPaymentOrderResponse {
    /// Non-zero means error.
    pub error_code: Option<i32>,
    /// Human-readable info / error description.
    pub info: Option<String>,
    /// If 3DS is required, the ACS URL to redirect to.
    pub acs_url: Option<String>,
    /// The CReq value for 3DS2 challenge.
    #[serde(rename = "cReq")]
    pub creq: Option<String>,
    /// Redirect URL (non-3DS success).
    pub redirect: Option<String>,
    /// Whether the payment was successful without 3DS.
    pub success: Option<bool>,
    /// Additional data blob.
    pub data: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Unified authorize response (we parse *both* register and paymentorder)
// ---------------------------------------------------------------------------

/// The main connector uses a two-step authorize flow:
///   1. `register.do` -> get orderId + formUrl
///   2. `paymentorder.do` -> submit card data -> possibly 3DS redirect
///
/// Because Hyperswitch calls `handle_response` once, we unify both response
/// shapes under this enum.  The connector code in `atbbank.rs` will call
/// register + paymentorder sequentially in its `build_request` /
/// `handle_response` pair, but the *first* call (register) returns
/// `AtbbankPaymentsResponse::Register` and the full authorize returns
/// `AtbbankPaymentsResponse::PaymentOrder`.  For the initial integration we
/// handle only register (redirect-based flow).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AtbbankPaymentsResponse {
    /// register.do response — contains orderId + formUrl for redirect.
    Register(AtbbankRegisterResponse),
    /// paymentorder.do response — contains 3DS or success info.
    PaymentOrder(AtbbankPaymentOrderResponse),
}

impl<F, T>
    TryFrom<ResponseRouterData<F, AtbbankPaymentsResponse, T, PaymentsResponseData>>
    for RouterData<F, T, PaymentsResponseData>
{
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(
        item: ResponseRouterData<F, AtbbankPaymentsResponse, T, PaymentsResponseData>,
    ) -> Result<Self, Self::Error> {
        match item.response {
            AtbbankPaymentsResponse::Register(ref register) => {
                let error_code = register.error_code.unwrap_or(0);
                if error_code != 0 {
                    // Registration failed
                    let message = register
                        .error_message
                        .clone()
                        .unwrap_or_else(|| format!("ATB error code: {error_code}"));
                    return Ok(Self {
                        status: enums::AttemptStatus::Failure,
                        response: Err(hyperswitch_domain_models::router_data::ErrorResponse {
                            code: error_code.to_string(),
                            message: message.clone(),
                            reason: Some(message),
                            status_code: item.http_code,
                            attempt_status: Some(enums::AttemptStatus::Failure),
                            connector_transaction_id: None,
                            connector_response_reference_id: None,
                            network_advice_code: None,
                            network_decline_code: None,
                            network_error_message: None,
                            connector_metadata: None,
                        }),
                        ..item.data
                    });
                }

                let order_id = register.order_id.clone().unwrap_or_default();
                let form_url = register.form_url.clone().unwrap_or_default();

                // Build redirect to the ATB payment page
                let redirection_data = if !form_url.is_empty() {
                    Some(RedirectForm::Form {
                        endpoint: form_url,
                        method: common_utils::request::Method::Get,
                        form_fields: HashMap::new(),
                    })
                } else {
                    None
                };

                let connector_metadata = Some(
                    serde_json::to_value(AtbbankMeta {
                        order_id: order_id.clone(),
                    })
                    .change_context(errors::ConnectorError::ResponseHandlingFailed)?,
                );

                Ok(Self {
                    status: enums::AttemptStatus::AuthenticationPending,
                    response: Ok(PaymentsResponseData::TransactionResponse {
                        resource_id: ResponseId::ConnectorTransactionId(order_id),
                        redirection_data: Box::new(redirection_data),
                        mandate_reference: Box::new(None),
                        connector_metadata,
                        network_txn_id: None,
                        connector_response_reference_id: None,
                        incremental_authorization_allowed: None,
                        authentication_data: None,
                        charges: None,
                    }),
                    ..item.data
                })
            }
            AtbbankPaymentsResponse::PaymentOrder(ref po) => {
                let error_code = po.error_code.unwrap_or(0);

                // Check for 3DS challenge
                if let (Some(ref acs_url), Some(ref creq)) = (&po.acs_url, &po.creq) {
                    if !acs_url.is_empty() && !creq.is_empty() {
                        let mut form_fields = HashMap::new();
                        form_fields.insert("creq".to_string(), creq.clone());

                        return Ok(Self {
                            status: enums::AttemptStatus::AuthenticationPending,
                            response: Ok(PaymentsResponseData::TransactionResponse {
                                resource_id: ResponseId::NoResponseId,
                                redirection_data: Box::new(Some(RedirectForm::Form {
                                    endpoint: acs_url.clone(),
                                    method: common_utils::request::Method::Post,
                                    form_fields,
                                })),
                                mandate_reference: Box::new(None),
                                connector_metadata: None,
                                network_txn_id: None,
                                connector_response_reference_id: None,
                                incremental_authorization_allowed: None,
                                authentication_data: None,
                                charges: None,
                            }),
                            ..item.data
                        });
                    }
                }

                // Non-3DS result
                if error_code != 0 {
                    let message = po
                        .info
                        .clone()
                        .unwrap_or_else(|| format!("ATB paymentorder error: {error_code}"));
                    return Ok(Self {
                        status: enums::AttemptStatus::Failure,
                        response: Err(hyperswitch_domain_models::router_data::ErrorResponse {
                            code: error_code.to_string(),
                            message: message.clone(),
                            reason: Some(message),
                            status_code: item.http_code,
                            attempt_status: Some(enums::AttemptStatus::Failure),
                            connector_transaction_id: None,
                            connector_response_reference_id: None,
                            network_advice_code: None,
                            network_decline_code: None,
                            network_error_message: None,
                            connector_metadata: None,
                        }),
                        ..item.data
                    });
                }

                // Success — check redirect or direct success
                if po.success == Some(true) {
                    Ok(Self {
                        status: enums::AttemptStatus::Charged,
                        response: Ok(PaymentsResponseData::TransactionResponse {
                            resource_id: ResponseId::NoResponseId,
                            redirection_data: Box::new(None),
                            mandate_reference: Box::new(None),
                            connector_metadata: None,
                            network_txn_id: None,
                            connector_response_reference_id: None,
                            incremental_authorization_allowed: None,
                            authentication_data: None,
                            charges: None,
                        }),
                        ..item.data
                    })
                } else if let Some(ref redirect_url) = po.redirect {
                    Ok(Self {
                        status: enums::AttemptStatus::AuthenticationPending,
                        response: Ok(PaymentsResponseData::TransactionResponse {
                            resource_id: ResponseId::NoResponseId,
                            redirection_data: Box::new(Some(RedirectForm::Form {
                                endpoint: redirect_url.clone(),
                                method: common_utils::request::Method::Get,
                                form_fields: HashMap::new(),
                            })),
                            mandate_reference: Box::new(None),
                            connector_metadata: None,
                            network_txn_id: None,
                            connector_response_reference_id: None,
                            incremental_authorization_allowed: None,
                            authentication_data: None,
                            charges: None,
                        }),
                        ..item.data
                    })
                } else {
                    // Ambiguous state — treat as pending
                    Ok(Self {
                        status: enums::AttemptStatus::Pending,
                        response: Ok(PaymentsResponseData::TransactionResponse {
                            resource_id: ResponseId::NoResponseId,
                            redirection_data: Box::new(None),
                            mandate_reference: Box::new(None),
                            connector_metadata: None,
                            network_txn_id: None,
                            connector_response_reference_id: None,
                            incremental_authorization_allowed: None,
                            authentication_data: None,
                            charges: None,
                        }),
                        ..item.data
                    })
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PSync — getOrderStatusExtended.do
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct AtbbankStatusRequest {
    #[serde(rename = "userName")]
    pub user_name: Secret<String>,
    pub password: Secret<String>,
    #[serde(rename = "orderId")]
    pub order_id: String,
    pub language: String,
}

impl TryFrom<&PaymentsSyncRouterData> for AtbbankStatusRequest {
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(item: &PaymentsSyncRouterData) -> Result<Self, Self::Error> {
        let auth = AtbbankAuthType::try_from(&item.connector_auth_type)?;
        let order_id = item
            .request
            .connector_transaction_id
            .get_connector_transaction_id()
            .change_context(errors::ConnectorError::MissingConnectorTransactionID)?;

        Ok(Self {
            user_name: auth.user_name,
            password: auth.password,
            order_id,
            language: "en".to_string(),
        })
    }
}

/// TryFrom for RefundSync — reuses the same getOrderStatusExtended.do endpoint.
impl TryFrom<&RefundSyncRouterData> for AtbbankStatusRequest {
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(item: &RefundSyncRouterData) -> Result<Self, Self::Error> {
        let auth = AtbbankAuthType::try_from(&item.connector_auth_type)?;
        // For refund sync, connector_transaction_id is the orderId of the original payment
        let order_id = item.request.connector_transaction_id.clone();

        Ok(Self {
            user_name: auth.user_name,
            password: auth.password,
            order_id,
            language: "en".to_string(),
        })
    }
}

/// SmartVista OrderStatus numeric values.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AtbbankOrderStatus {
    /// 0 = Registered (order created, not yet paid)
    Registered = 0,
    /// 1 = Authorized (pre-auth hold placed)
    Authorized = 1,
    /// 2 = Deposited (captured / auto-captured)
    Deposited = 2,
    /// 3 = Reversed (voided)
    Reversed = 3,
    /// 4 = Refunded
    Refunded = 4,
    /// 6 = Declined
    Declined = 6,
    /// 7 = System error
    SystemError = 7,
}

impl AtbbankOrderStatus {
    fn from_i32(value: i32) -> Option<Self> {
        match value {
            0 => Some(Self::Registered),
            1 => Some(Self::Authorized),
            2 => Some(Self::Deposited),
            3 => Some(Self::Reversed),
            4 => Some(Self::Refunded),
            6 => Some(Self::Declined),
            7 => Some(Self::SystemError),
            _ => None,
        }
    }
}

/// Map ATB order status to Hyperswitch attempt status.
/// `is_auto_capture` distinguishes between pre-auth (manual) and one-step flows.
fn atb_status_to_attempt_status(
    status: AtbbankOrderStatus,
    is_auto_capture: bool,
) -> enums::AttemptStatus {
    match status {
        AtbbankOrderStatus::Registered => enums::AttemptStatus::AuthenticationPending,
        AtbbankOrderStatus::Authorized => {
            if is_auto_capture {
                // One-step: authorized = effectively charged (deposit pending)
                enums::AttemptStatus::Charged
            } else {
                enums::AttemptStatus::Authorized
            }
        }
        AtbbankOrderStatus::Deposited => enums::AttemptStatus::Charged,
        AtbbankOrderStatus::Reversed => enums::AttemptStatus::Voided,
        // Refunded orders are still "Charged" from the payment perspective;
        // refund status is tracked separately.
        AtbbankOrderStatus::Refunded => enums::AttemptStatus::Charged,
        AtbbankOrderStatus::Declined => enums::AttemptStatus::Failure,
        AtbbankOrderStatus::SystemError => enums::AttemptStatus::Failure,
    }
}

/// Response from `getOrderStatusExtended.do`.
/// Note: field names use PascalCase in SmartVista responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AtbbankOrderStatusResponse {
    /// Numeric order status (0-7).
    #[serde(rename = "orderStatus")]
    pub order_status: Option<i32>,
    /// Error code as STRING (unlike register.do which is numeric).
    #[serde(rename = "ErrorCode")]
    pub error_code: Option<String>,
    #[serde(rename = "ErrorMessage")]
    pub error_message: Option<String>,
    #[serde(rename = "OrderNumber")]
    pub order_number: Option<String>,
    #[serde(rename = "Pan")]
    pub pan: Option<String>,
    #[serde(rename = "Amount")]
    pub amount: Option<i64>,
    #[serde(rename = "Currency")]
    pub currency: Option<String>,
    pub action_code: Option<i32>,
    pub action_code_description: Option<String>,
    pub deposit_amount: Option<i64>,
    pub refunded_amount: Option<i64>,
    pub approval_code: Option<String>,
    pub auth_code: Option<String>,
    pub ip: Option<String>,
    pub card_auth_info: Option<serde_json::Value>,
    pub payment_amount_info: Option<serde_json::Value>,
}

impl<F, T>
    TryFrom<
        ResponseRouterData<F, AtbbankOrderStatusResponse, T, PaymentsResponseData>,
    > for RouterData<F, T, PaymentsResponseData>
{
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(
        item: ResponseRouterData<F, AtbbankOrderStatusResponse, T, PaymentsResponseData>,
    ) -> Result<Self, Self::Error> {
        let response = &item.response;

        // Check for error first
        let error_code = response.error_code.clone().unwrap_or_default();
        if !error_code.is_empty() && error_code != "0" {
            let message = response
                .error_message
                .clone()
                .unwrap_or_else(|| format!("ATB status error: {error_code}"));
            return Ok(Self {
                status: enums::AttemptStatus::Failure,
                response: Err(hyperswitch_domain_models::router_data::ErrorResponse {
                    code: error_code,
                    message: message.clone(),
                    reason: Some(message),
                    status_code: item.http_code,
                    attempt_status: Some(enums::AttemptStatus::Failure),
                    connector_transaction_id: None,
                    connector_response_reference_id: response.order_number.clone(),
                    network_advice_code: None,
                    network_decline_code: None,
                    network_error_message: response.action_code_description.clone(),
                    connector_metadata: None,
                }),
                ..item.data
            });
        }

        let order_status_int = response.order_status.unwrap_or(0);
        let order_status = AtbbankOrderStatus::from_i32(order_status_int).unwrap_or(
            AtbbankOrderStatus::Registered,
        );

        // Default to auto-capture for status sync (conservative)
        let status = atb_status_to_attempt_status(order_status, true);

        let connector_metadata = response.order_number.as_ref().map(|_| {
            serde_json::json!({
                "action_code": response.action_code,
                "action_code_description": response.action_code_description,
                "approval_code": response.approval_code,
                "auth_code": response.auth_code,
                "deposit_amount": response.deposit_amount,
                "refunded_amount": response.refunded_amount,
            })
        });

        Ok(Self {
            status,
            response: Ok(PaymentsResponseData::TransactionResponse {
                resource_id: ResponseId::ConnectorTransactionId(
                    response.order_number.clone().unwrap_or_default(),
                ),
                redirection_data: Box::new(None),
                mandate_reference: Box::new(None),
                connector_metadata,
                network_txn_id: None,
                connector_response_reference_id: response.order_number.clone(),
                incremental_authorization_allowed: None,
                authentication_data: None,
                charges: None,
            }),
            ..item.data
        })
    }
}

// ---------------------------------------------------------------------------
// Capture — deposit.do  (form-urlencoded)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct AtbbankCaptureRequest {
    #[serde(rename = "userName")]
    pub user_name: Secret<String>,
    pub password: Secret<String>,
    #[serde(rename = "orderId")]
    pub order_id: String,
    /// Amount in minor units to capture.
    pub amount: String,
}

impl TryFrom<&PaymentsCaptureRouterData> for AtbbankCaptureRequest {
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(item: &PaymentsCaptureRouterData) -> Result<Self, Self::Error> {
        let auth = AtbbankAuthType::try_from(&item.connector_auth_type)?;
        let order_id = item.request.connector_transaction_id.clone();
        let amount = item.request.minor_amount_to_capture.to_string();

        Ok(Self {
            user_name: auth.user_name,
            password: auth.password,
            order_id,
            amount,
        })
    }
}

impl TryFrom<(&PaymentsCaptureRouterData, StringMinorUnit)> for AtbbankCaptureRequest {
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(
        (item, amount): (&PaymentsCaptureRouterData, StringMinorUnit),
    ) -> Result<Self, Self::Error> {
        let auth = AtbbankAuthType::try_from(&item.connector_auth_type)?;
        let order_id = item.request.connector_transaction_id.clone();

        Ok(Self {
            user_name: auth.user_name,
            password: auth.password,
            order_id,
            amount: amount.to_string(),
        })
    }
}

/// Response from deposit.do — same shape as register error response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AtbbankCaptureResponse {
    pub error_code: Option<i32>,
    pub error_message: Option<String>,
}

impl TryFrom<PaymentsCaptureResponseRouterData<AtbbankCaptureResponse>>
    for PaymentsCaptureRouterData
{
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(
        item: PaymentsCaptureResponseRouterData<AtbbankCaptureResponse>,
    ) -> Result<Self, Self::Error> {
        let error_code = item.response.error_code.unwrap_or(0);
        if error_code != 0 {
            let message = item
                .response
                .error_message
                .clone()
                .unwrap_or_else(|| format!("ATB capture error: {error_code}"));
            return Ok(Self {
                status: enums::AttemptStatus::Failure,
                response: Err(hyperswitch_domain_models::router_data::ErrorResponse {
                    code: error_code.to_string(),
                    message: message.clone(),
                    reason: Some(message),
                    status_code: item.http_code,
                    attempt_status: Some(enums::AttemptStatus::CaptureFailed),
                    connector_transaction_id: None,
                    connector_response_reference_id: None,
                    network_advice_code: None,
                    network_decline_code: None,
                    network_error_message: None,
                    connector_metadata: None,
                }),
                amount_captured: None,
                ..item.data
            });
        }

        Ok(Self {
            status: enums::AttemptStatus::Charged,
            response: Ok(PaymentsResponseData::TransactionResponse {
                resource_id: ResponseId::ConnectorTransactionId(
                    item.data.request.connector_transaction_id.clone(),
                ),
                redirection_data: Box::new(None),
                mandate_reference: Box::new(None),
                connector_metadata: None,
                network_txn_id: None,
                connector_response_reference_id: None,
                incremental_authorization_allowed: None,
                authentication_data: None,
                charges: None,
            }),
            amount_captured: None,
            ..item.data
        })
    }
}

/// TryFrom for void (cancel) context when using AtbbankCaptureResponse via
/// the AtbbankActionResponse alias. Both capture and void endpoints return
/// the same `{errorCode, errorMessage}` shape from SmartVista.
impl
    TryFrom<PaymentsCancelResponseRouterData<AtbbankCaptureResponse>>
    for hyperswitch_domain_models::types::PaymentsCancelRouterData
{
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(
        item: PaymentsCancelResponseRouterData<AtbbankCaptureResponse>,
    ) -> Result<Self, Self::Error> {
        let error_code = item.response.error_code.unwrap_or(0);
        if error_code != 0 {
            let message = item
                .response
                .error_message
                .clone()
                .unwrap_or_else(|| format!("ATB reverse error: {error_code}"));
            return Ok(Self {
                status: enums::AttemptStatus::VoidFailed,
                response: Err(hyperswitch_domain_models::router_data::ErrorResponse {
                    code: error_code.to_string(),
                    message: message.clone(),
                    reason: Some(message),
                    status_code: item.http_code,
                    attempt_status: Some(enums::AttemptStatus::VoidFailed),
                    connector_transaction_id: None,
                    connector_response_reference_id: None,
                    network_advice_code: None,
                    network_decline_code: None,
                    network_error_message: None,
                    connector_metadata: None,
                }),
                ..item.data
            });
        }

        Ok(Self {
            status: enums::AttemptStatus::Voided,
            response: Ok(PaymentsResponseData::TransactionResponse {
                resource_id: ResponseId::NoResponseId,
                redirection_data: Box::new(None),
                mandate_reference: Box::new(None),
                connector_metadata: None,
                network_txn_id: None,
                connector_response_reference_id: None,
                incremental_authorization_allowed: None,
                authentication_data: None,
                charges: None,
            }),
            ..item.data
        })
    }
}

// ---------------------------------------------------------------------------
// Void / Reverse — reverse.do  (form-urlencoded)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct AtbbankReverseRequest {
    #[serde(rename = "userName")]
    pub user_name: Secret<String>,
    pub password: Secret<String>,
    #[serde(rename = "orderId")]
    pub order_id: String,
}

impl TryFrom<&hyperswitch_domain_models::types::PaymentsCancelRouterData>
    for AtbbankReverseRequest
{
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(
        item: &hyperswitch_domain_models::types::PaymentsCancelRouterData,
    ) -> Result<Self, Self::Error> {
        let auth = AtbbankAuthType::try_from(&item.connector_auth_type)?;
        let order_id = item.request.connector_transaction_id.clone();

        Ok(Self {
            user_name: auth.user_name,
            password: auth.password,
            order_id,
        })
    }
}

/// Response from reverse.do.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AtbbankReverseResponse {
    pub error_code: Option<i32>,
    pub error_message: Option<String>,
}

impl<F, T>
    TryFrom<ResponseRouterData<F, AtbbankReverseResponse, T, PaymentsResponseData>>
    for RouterData<F, T, PaymentsResponseData>
{
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(
        item: ResponseRouterData<F, AtbbankReverseResponse, T, PaymentsResponseData>,
    ) -> Result<Self, Self::Error> {
        let error_code = item.response.error_code.unwrap_or(0);
        if error_code != 0 {
            let message = item
                .response
                .error_message
                .clone()
                .unwrap_or_else(|| format!("ATB reverse error: {error_code}"));
            return Ok(Self {
                status: enums::AttemptStatus::VoidFailed,
                response: Err(hyperswitch_domain_models::router_data::ErrorResponse {
                    code: error_code.to_string(),
                    message: message.clone(),
                    reason: Some(message),
                    status_code: item.http_code,
                    attempt_status: Some(enums::AttemptStatus::VoidFailed),
                    connector_transaction_id: None,
                    connector_response_reference_id: None,
                    network_advice_code: None,
                    network_decline_code: None,
                    network_error_message: None,
                    connector_metadata: None,
                }),
                ..item.data
            });
        }

        Ok(Self {
            status: enums::AttemptStatus::Voided,
            response: Ok(PaymentsResponseData::TransactionResponse {
                resource_id: ResponseId::NoResponseId,
                redirection_data: Box::new(None),
                mandate_reference: Box::new(None),
                connector_metadata: None,
                network_txn_id: None,
                connector_response_reference_id: None,
                incremental_authorization_allowed: None,
                authentication_data: None,
                charges: None,
            }),
            ..item.data
        })
    }
}

// ---------------------------------------------------------------------------
// Refund — refund.do  (form-urlencoded)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct AtbbankRefundRequest {
    #[serde(rename = "userName")]
    pub user_name: Secret<String>,
    pub password: Secret<String>,
    #[serde(rename = "orderId")]
    pub order_id: String,
    /// Amount to refund in minor units.
    pub amount: String,
}

impl<F> TryFrom<&AtbbankRouterData<&RefundsRouterData<F>>> for AtbbankRefundRequest {
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(
        item: &AtbbankRouterData<&RefundsRouterData<F>>,
    ) -> Result<Self, Self::Error> {
        let router_data = item.router_data;
        let auth = AtbbankAuthType::try_from(&router_data.connector_auth_type)?;
        let order_id = router_data.request.connector_transaction_id.clone();

        Ok(Self {
            user_name: auth.user_name,
            password: auth.password,
            order_id,
            amount: item.amount.to_string(),
        })
    }
}

/// Response from refund.do.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AtbbankRefundResponse {
    pub error_code: Option<i32>,
    pub error_message: Option<String>,
}

impl From<&AtbbankRefundResponse> for enums::RefundStatus {
    fn from(item: &AtbbankRefundResponse) -> Self {
        let error_code = item.error_code.unwrap_or(0);
        if error_code == 0 {
            Self::Success
        } else {
            Self::Failure
        }
    }
}

impl TryFrom<RefundsResponseRouterData<Execute, AtbbankRefundResponse>>
    for RefundsRouterData<Execute>
{
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(
        item: RefundsResponseRouterData<Execute, AtbbankRefundResponse>,
    ) -> Result<Self, Self::Error> {
        let refund_status = enums::RefundStatus::from(&item.response);
        if refund_status == enums::RefundStatus::Failure {
            let message = item
                .response
                .error_message
                .clone()
                .unwrap_or_else(|| "ATB refund failed".to_string());
            return Ok(Self {
                response: Err(hyperswitch_domain_models::router_data::ErrorResponse {
                    code: item
                        .response
                        .error_code
                        .map(|c| c.to_string())
                        .unwrap_or_default(),
                    message: message.clone(),
                    reason: Some(message),
                    status_code: item.http_code,
                    attempt_status: None,
                    connector_transaction_id: None,
                    connector_response_reference_id: None,
                    network_advice_code: None,
                    network_decline_code: None,
                    network_error_message: None,
                    connector_metadata: None,
                }),
                ..item.data
            });
        }

        Ok(Self {
            response: Ok(RefundsResponseData {
                // ATB refund.do does not return a separate refund ID;
                // we use the connector_transaction_id (orderId) as reference.
                connector_refund_id: item.data.request.connector_transaction_id.clone(),
                refund_status,
            }),
            ..item.data
        })
    }
}

// ---------------------------------------------------------------------------
// Refund Sync — uses getOrderStatusExtended.do (same as PSync)
// ---------------------------------------------------------------------------

/// For refund sync we reuse `AtbbankOrderStatusResponse`.  The refund status
/// is derived from the `refundedAmount` field compared to the requested amount.
impl TryFrom<RefundsResponseRouterData<RSync, AtbbankOrderStatusResponse>>
    for RefundsRouterData<RSync>
{
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(
        item: RefundsResponseRouterData<RSync, AtbbankOrderStatusResponse>,
    ) -> Result<Self, Self::Error> {
        let response = &item.response;
        let order_status_int = response.order_status.unwrap_or(0);

        let refund_status = match AtbbankOrderStatus::from_i32(order_status_int) {
            Some(AtbbankOrderStatus::Refunded) => enums::RefundStatus::Success,
            Some(AtbbankOrderStatus::Declined) | Some(AtbbankOrderStatus::SystemError) => {
                enums::RefundStatus::Failure
            }
            _ => {
                // If refundedAmount > 0 but order isn't fully in "Refunded" state,
                // partial refund may be pending.
                if response.refunded_amount.unwrap_or(0) > 0 {
                    enums::RefundStatus::Success
                } else {
                    enums::RefundStatus::Pending
                }
            }
        };

        Ok(Self {
            response: Ok(RefundsResponseData {
                connector_refund_id: response
                    .order_number
                    .clone()
                    .unwrap_or_default(),
                refund_status,
            }),
            ..item.data
        })
    }
}

// ---------------------------------------------------------------------------
// Error response
// ---------------------------------------------------------------------------

#[derive(Default, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AtbbankErrorResponse {
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

// ---------------------------------------------------------------------------
// Type aliases for atbbank.rs compatibility
// ---------------------------------------------------------------------------

/// register.do request (Authorize flow).
pub type AtbbankPaymentsRequest = AtbbankRegisterRequest;
/// paymentorder.do request (CompleteAuthorize flow — card data after 3DS redirect).
pub type AtbbankCompleteAuthorizeRequest = AtbbankPaymentOrderRequest;
/// getOrderStatusExtended.do request (PSync flow).
pub type AtbbankSyncRequest = AtbbankStatusRequest;
/// getOrderStatusExtended.do response (PSync flow).
pub type AtbbankSyncResponse = AtbbankOrderStatusResponse;
/// Unified action response for deposit.do and reverse.do — both return
/// `{errorCode, errorMessage}` from SmartVista.
pub type AtbbankActionResponse = AtbbankCaptureResponse;
/// reverse.do request (Void flow).
pub type AtbbankVoidRequest = AtbbankReverseRequest;
/// refund.do response.
pub type RefundResponse = AtbbankRefundResponse;
/// getOrderStatusExtended.do request for refund sync (same endpoint as PSync).
pub type AtbbankRefundSyncRequest = AtbbankStatusRequest;
