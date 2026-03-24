pub mod transformers;

use std::collections::HashMap;
use std::sync::LazyLock;

use common_enums::enums;
use common_utils::{
    errors::CustomResult,
    ext_traits::BytesExt,
    request::{Method, Request, RequestBuilder, RequestContent},
    types::{AmountConvertor, StringMinorUnit, StringMinorUnitForConnector},
};
use error_stack::{report, ResultExt};
use hyperswitch_domain_models::{
    router_data::{AccessToken, ConnectorAuthType, ErrorResponse, RouterData},
    router_flow_types::{
        access_token_auth::AccessTokenAuth,
        payments::{
            Authorize, Capture, CompleteAuthorize, PSync, PaymentMethodToken, Session,
            SetupMandate, Void,
        },
        refunds::{Execute, RSync},
    },
    router_request_types::{
        AccessTokenRequestData, CompleteAuthorizeData, PaymentMethodTokenizationData,
        PaymentsAuthorizeData, PaymentsCancelData, PaymentsCaptureData, PaymentsSessionData,
        PaymentsSyncData, RefundsData, SetupMandateRequestData,
    },
    router_response_types::{
        ConnectorInfo, PaymentMethodDetails, PaymentsResponseData, RefundsResponseData,
        SupportedPaymentMethods, SupportedPaymentMethodsExt,
    },
    types::{
        PaymentsAuthorizeRouterData, PaymentsCancelRouterData, PaymentsCaptureRouterData,
        PaymentsCompleteAuthorizeRouterData, PaymentsSyncRouterData, RefundSyncRouterData,
        RefundsRouterData,
    },
};
use hyperswitch_interfaces::{
    api::{
        self, ConnectorCommon, ConnectorCommonExt, ConnectorIntegration, ConnectorSpecifications,
        ConnectorValidation,
    },
    configs::Connectors,
    errors,
    events::connector_api_logs::ConnectorEvent,
    types::{self, Response},
    webhooks::{IncomingWebhook, IncomingWebhookRequestDetails, WebhookContext},
};
use masking::{Mask, PeekInterface, Secret};
use transformers as atbbank;

use crate::{
    constants::headers,
    types::ResponseRouterData,
    utils,
};

#[derive(Clone)]
pub struct Atbbank {
    amount_converter: &'static (dyn AmountConvertor<Output = StringMinorUnit> + Sync),
}

impl Atbbank {
    pub fn new() -> &'static Self {
        &Self {
            amount_converter: &StringMinorUnitForConnector,
        }
    }
}

impl api::Payment for Atbbank {}
impl api::PaymentSession for Atbbank {}
impl api::ConnectorAccessToken for Atbbank {}
impl api::MandateSetup for Atbbank {}
impl api::PaymentAuthorize for Atbbank {}
impl api::PaymentSync for Atbbank {}
impl api::PaymentCapture for Atbbank {}
impl api::PaymentVoid for Atbbank {}
impl api::Refund for Atbbank {}
impl api::RefundExecute for Atbbank {}
impl api::RefundSync for Atbbank {}
impl api::PaymentToken for Atbbank {}
impl api::PaymentsCompleteAuthorize for Atbbank {}

impl ConnectorIntegration<PaymentMethodToken, PaymentMethodTokenizationData, PaymentsResponseData>
    for Atbbank
{
    // Not Implemented
}

impl<Flow, Request, Response> ConnectorCommonExt<Flow, Request, Response> for Atbbank
where
    Self: ConnectorIntegration<Flow, Request, Response>,
{
    fn build_headers(
        &self,
        _req: &RouterData<Flow, Request, Response>,
        _connectors: &Connectors,
    ) -> CustomResult<Vec<(String, masking::Maskable<String>)>, errors::ConnectorError> {
        // ATB SmartVista: auth credentials go in the request body, not headers.
        // Only Content-Type header is needed.
        Ok(vec![(
            headers::CONTENT_TYPE.to_string(),
            self.get_content_type().to_string().into(),
        )])
    }
}

impl ConnectorCommon for Atbbank {
    fn id(&self) -> &'static str {
        "atbbank"
    }

    fn get_currency_unit(&self) -> api::CurrencyUnit {
        // SmartVista EPG processes amounts in minor units (qepik for AZN, kopecks for RUB, cents for USD/EUR)
        api::CurrencyUnit::Minor
    }

    fn common_get_content_type(&self) -> &'static str {
        "application/x-www-form-urlencoded"
    }

    fn base_url<'a>(&self, connectors: &'a Connectors) -> &'a str {
        connectors.atbbank.base_url.as_ref()
    }

    fn get_auth_header(
        &self,
        _auth_type: &ConnectorAuthType,
    ) -> CustomResult<Vec<(String, masking::Maskable<String>)>, errors::ConnectorError> {
        // ATB SmartVista sends credentials in request body (userName/password fields), not in headers
        Ok(vec![])
    }

    fn build_error_response(
        &self,
        res: Response,
        event_builder: Option<&mut ConnectorEvent>,
    ) -> CustomResult<ErrorResponse, errors::ConnectorError> {
        let response: atbbank::AtbbankErrorResponse = res
            .response
            .parse_struct("AtbbankErrorResponse")
            .change_context(errors::ConnectorError::ResponseDeserializationFailed)?;

        event_builder.map(|i| i.set_error_response_body(&response));
        router_env::logger::info!(connector_response=?response);

        let error_message = response
            .error_message
            .unwrap_or_else(|| "Unknown error".to_string());
        let error_code = response
            .error_code
            .unwrap_or_else(|| "UNKNOWN".to_string());

        Ok(ErrorResponse {
            status_code: res.status_code,
            code: error_code,
            message: error_message.clone(),
            reason: Some(error_message),
            attempt_status: None,
            connector_transaction_id: None,
            connector_response_reference_id: None,
            network_advice_code: None,
            network_decline_code: None,
            network_error_message: None,
            connector_metadata: None,
        })
    }
}

impl ConnectorValidation for Atbbank {
    fn validate_psync_reference_id(
        &self,
        _data: &PaymentsSyncData,
        _is_three_ds: bool,
        _status: enums::AttemptStatus,
        _connector_meta_data: Option<common_utils::pii::SecretSerdeValue>,
    ) -> CustomResult<(), errors::ConnectorError> {
        // ATB uses orderId from connector metadata, not the standard connector_transaction_id
        Ok(())
    }
}

impl ConnectorIntegration<Session, PaymentsSessionData, PaymentsResponseData> for Atbbank {
    // Session flow not supported by ATB SmartVista EPG
}

impl ConnectorIntegration<AccessTokenAuth, AccessTokenRequestData, AccessToken> for Atbbank {
    // Access token not required - ATB uses username/password in each request body
}

impl ConnectorIntegration<SetupMandate, SetupMandateRequestData, PaymentsResponseData>
    for Atbbank
{
    fn build_request(
        &self,
        _req: &RouterData<SetupMandate, SetupMandateRequestData, PaymentsResponseData>,
        _connectors: &Connectors,
    ) -> CustomResult<Option<Request>, errors::ConnectorError> {
        Err(
            errors::ConnectorError::NotImplemented("Setup Mandate flow for Atbbank".to_string())
                .into(),
        )
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// PaymentAuthorize: POST /rest/register.do (one-step) or /rest/registerPreAuth.do (pre-auth)
// Registers an order with ATB SmartVista EPG and returns orderId + formUrl
// ──────────────────────────────────────────────────────────────────────────────

impl ConnectorIntegration<Authorize, PaymentsAuthorizeData, PaymentsResponseData> for Atbbank {
    fn get_headers(
        &self,
        req: &PaymentsAuthorizeRouterData,
        connectors: &Connectors,
    ) -> CustomResult<Vec<(String, masking::Maskable<String>)>, errors::ConnectorError> {
        self.build_headers(req, connectors)
    }

    fn get_content_type(&self) -> &'static str {
        self.common_get_content_type()
    }

    fn get_url(
        &self,
        req: &PaymentsAuthorizeRouterData,
        connectors: &Connectors,
    ) -> CustomResult<String, errors::ConnectorError> {
        let base = self.base_url(connectors);
        // Use registerPreAuth for manual capture, register for automatic
        let path = if req.request.capture_method == Some(enums::CaptureMethod::Manual) {
            "rest/registerPreAuth.do"
        } else {
            "rest/register.do"
        };
        Ok(format!("{}{}", base, path))
    }

    fn get_request_body(
        &self,
        req: &PaymentsAuthorizeRouterData,
        _connectors: &Connectors,
    ) -> CustomResult<RequestContent, errors::ConnectorError> {
        let amount = utils::convert_amount(
            self.amount_converter,
            req.request.minor_amount,
            req.request.currency,
        )?;

        let connector_router_data = atbbank::AtbbankRouterData::from((amount, req));
        let connector_req =
            atbbank::AtbbankPaymentsRequest::try_from(&connector_router_data)?;
        Ok(RequestContent::FormUrlEncoded(Box::new(connector_req)))
    }

    fn build_request(
        &self,
        req: &PaymentsAuthorizeRouterData,
        connectors: &Connectors,
    ) -> CustomResult<Option<Request>, errors::ConnectorError> {
        Ok(Some(
            RequestBuilder::new()
                .method(Method::Post)
                .url(&types::PaymentsAuthorizeType::get_url(
                    self, req, connectors,
                )?)
                .attach_default_headers()
                .headers(types::PaymentsAuthorizeType::get_headers(
                    self, req, connectors,
                )?)
                .set_body(types::PaymentsAuthorizeType::get_request_body(
                    self, req, connectors,
                )?)
                .build(),
        ))
    }

    fn handle_response(
        &self,
        data: &PaymentsAuthorizeRouterData,
        event_builder: Option<&mut ConnectorEvent>,
        res: Response,
    ) -> CustomResult<PaymentsAuthorizeRouterData, errors::ConnectorError> {
        let response: atbbank::AtbbankPaymentsResponse = res
            .response
            .parse_struct("AtbbankPaymentsResponse")
            .change_context(errors::ConnectorError::ResponseDeserializationFailed)?;

        event_builder.map(|i| i.set_response_body(&response));
        router_env::logger::info!(connector_response=?response);

        let mut router_data = RouterData::try_from(ResponseRouterData {
            response,
            data: data.clone(),
            http_code: res.status_code,
        })?;

        // ATB H2H flow: after register.do, redirect to Hyperswitch's
        // CompleteAuthorize endpoint instead of ATB's hosted payment page.
        // Also save card data in connector_metadata so CompleteAuthorize
        // can use it (payment_method_data is not available via redirect callback).
        if let Ok(PaymentsResponseData::TransactionResponse {
            ref mut redirection_data,
            ref mut connector_metadata,
            ..
        }) = router_data.response
        {
            // Redirect to CompleteAuthorize URL instead of ATB hosted page
            if let Some(ref complete_url) = data.request.complete_authorize_url {
                if let Some(ref mut redirect) = **redirection_data {
                    use hyperswitch_domain_models::router_response_types::RedirectForm;
                    *redirect = RedirectForm::Form {
                        endpoint: complete_url.clone(),
                        method: Method::Get,
                        form_fields: HashMap::new(),
                    };
                }
            }

            // Save card data in connector_metadata for H2H CompleteAuthorize flow
            if let Some(ref meta_value) = connector_metadata {
                if let Ok(mut meta) =
                    serde_json::from_value::<atbbank::AtbbankMeta>(meta_value.clone())
                {
                    use hyperswitch_domain_models::payment_method_data::PaymentMethodData;
                    if let PaymentMethodData::Card(ref card) = data.request.payment_method_data {
                        meta.card_number =
                            Some(Secret::new(card.card_number.get_card_no()));
                        meta.card_cvc = Some(card.card_cvc.clone());
                        meta.card_exp_month = Some(card.card_exp_month.clone());
                        meta.card_exp_year = Some(card.card_exp_year.clone());
                        meta.card_holder = card
                            .card_holder_name
                            .as_ref()
                            .map(|n| n.peek().to_string());

                        if let Ok(updated) = serde_json::to_value(&meta) {
                            *connector_metadata = Some(updated);
                        }
                    }
                }
            }
        }

        Ok(router_data)
    }

    fn get_error_response(
        &self,
        res: Response,
        event_builder: Option<&mut ConnectorEvent>,
    ) -> CustomResult<ErrorResponse, errors::ConnectorError> {
        self.build_error_response(res, event_builder)
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// CompleteAuthorize: POST /rest/paymentorder.do
// Used after 3DS redirect callback to complete the payment with card data or cRes
// ──────────────────────────────────────────────────────────────────────────────

impl ConnectorIntegration<CompleteAuthorize, CompleteAuthorizeData, PaymentsResponseData>
    for Atbbank
{
    fn get_headers(
        &self,
        req: &PaymentsCompleteAuthorizeRouterData,
        connectors: &Connectors,
    ) -> CustomResult<Vec<(String, masking::Maskable<String>)>, errors::ConnectorError> {
        self.build_headers(req, connectors)
    }

    fn get_content_type(&self) -> &'static str {
        self.common_get_content_type()
    }

    fn get_url(
        &self,
        _req: &PaymentsCompleteAuthorizeRouterData,
        connectors: &Connectors,
    ) -> CustomResult<String, errors::ConnectorError> {
        Ok(format!("{}rest/paymentorder.do", self.base_url(connectors)))
    }

    fn get_request_body(
        &self,
        req: &PaymentsCompleteAuthorizeRouterData,
        _connectors: &Connectors,
    ) -> CustomResult<RequestContent, errors::ConnectorError> {
        // Check if this is Stage 2 of two-stage 3DS2 flow
        // (threeDSServerTransId present in connector_meta from Stage 1)
        let meta: Option<atbbank::AtbbankMeta> = req
            .request
            .connector_meta
            .as_ref()
            .and_then(|m| serde_json::from_value(m.clone()).ok());

        if let Some(ref ts_id) = meta.as_ref().and_then(|m| m.three_ds_server_trans_id.clone()) {
            // Stage 2: send threeDSServerTransId (no card data)
            let auth = atbbank::AtbbankAuthType::try_from(&req.connector_auth_type)?;
            let order_id = meta.as_ref().map(|m| m.order_id.clone()).unwrap_or_default();
            let stage2_req = atbbank::AtbbankPaymentOrderStage2Request {
                user_name: auth.user_name,
                password: auth.password,
                mdorder: order_id,
                three_ds_server_trans_id: ts_id.clone(),
                language: "en".to_string(),
            };
            Ok(RequestContent::FormUrlEncoded(Box::new(stage2_req)))
        } else {
            // Stage 1: send card data (existing flow)
            let connector_req = atbbank::AtbbankCompleteAuthorizeRequest::try_from(req)?;
            Ok(RequestContent::FormUrlEncoded(Box::new(connector_req)))
        }
    }

    fn build_request(
        &self,
        req: &PaymentsCompleteAuthorizeRouterData,
        connectors: &Connectors,
    ) -> CustomResult<Option<Request>, errors::ConnectorError> {
        Ok(Some(
            RequestBuilder::new()
                .method(Method::Post)
                .url(&types::PaymentsCompleteAuthorizeType::get_url(
                    self, req, connectors,
                )?)
                .attach_default_headers()
                .headers(types::PaymentsCompleteAuthorizeType::get_headers(
                    self, req, connectors,
                )?)
                .set_body(types::PaymentsCompleteAuthorizeType::get_request_body(
                    self, req, connectors,
                )?)
                .build(),
        ))
    }

    fn handle_response(
        &self,
        data: &PaymentsCompleteAuthorizeRouterData,
        event_builder: Option<&mut ConnectorEvent>,
        res: Response,
    ) -> CustomResult<PaymentsCompleteAuthorizeRouterData, errors::ConnectorError> {
        let response: atbbank::AtbbankPaymentsResponse = res
            .response
            .parse_struct("AtbbankCompleteAuthorizeResponse")
            .change_context(errors::ConnectorError::ResponseDeserializationFailed)?;

        event_builder.map(|i| i.set_response_body(&response));
        router_env::logger::info!(connector_response=?response);

        let mut router_data = RouterData::try_from(ResponseRouterData {
            response,
            data: data.clone(),
            http_code: res.status_code,
        })?;

        // ── Stage 1 of two-stage 3DS2: redirect back to CompleteAuthorize ──
        // After TryFrom, if response was Stage 1 (threeDSServerTransId in new metadata,
        // no redirect set), we need to:
        // 1. Merge threeDSServerTransId into the existing connector_metadata (preserve order_id)
        // 2. Set redirect to complete_authorize_url to trigger Stage 2
        if let Ok(PaymentsResponseData::TransactionResponse {
            ref mut redirection_data,
            ref mut connector_metadata,
            ..
        }) = router_data.response
        {
            // Check if this is Stage 1 response (has threeDSServerTransId in new metadata, no redirect)
            let is_stage1 = redirection_data.is_none()
                && connector_metadata
                    .as_ref()
                    .and_then(|m| m.get("three_ds_server_trans_id"))
                    .is_some();

            if is_stage1 {
                let ts_id = connector_metadata
                    .as_ref()
                    .and_then(|m| m.get("three_ds_server_trans_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();

                // Rebuild full metadata: preserve order_id from original meta, add threeDSServerTransId
                let original_meta: Option<atbbank::AtbbankMeta> = data
                    .request
                    .connector_meta
                    .as_ref()
                    .and_then(|m| serde_json::from_value(m.clone()).ok());

                if let Some(mut meta) = original_meta {
                    meta.three_ds_server_trans_id = Some(ts_id);
                    // Clear card data — no longer needed after Stage 1
                    meta.card_number = None;
                    meta.card_cvc = None;
                    meta.card_exp_month = None;
                    meta.card_exp_year = None;
                    meta.card_holder = None;

                    if let Ok(updated) = serde_json::to_value(&meta) {
                        *connector_metadata = Some(updated);
                    }
                }

                // Redirect to complete_authorize_url to trigger Stage 2
                if let Some(ref complete_url) = data.request.complete_authorize_url {
                    use hyperswitch_domain_models::router_response_types::RedirectForm;
                    *redirection_data = Box::new(Some(RedirectForm::Form {
                        endpoint: complete_url.clone(),
                        method: Method::Get,
                        form_fields: HashMap::new(),
                    }));
                }
            }
        }

        Ok(router_data)
    }

    fn get_error_response(
        &self,
        res: Response,
        event_builder: Option<&mut ConnectorEvent>,
    ) -> CustomResult<ErrorResponse, errors::ConnectorError> {
        self.build_error_response(res, event_builder)
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// PaymentSync: POST /rest/getOrderStatusExtended.do
// ──────────────────────────────────────────────────────────────────────────────

impl ConnectorIntegration<PSync, PaymentsSyncData, PaymentsResponseData> for Atbbank {
    fn get_headers(
        &self,
        req: &PaymentsSyncRouterData,
        connectors: &Connectors,
    ) -> CustomResult<Vec<(String, masking::Maskable<String>)>, errors::ConnectorError> {
        self.build_headers(req, connectors)
    }

    fn get_content_type(&self) -> &'static str {
        self.common_get_content_type()
    }

    fn get_url(
        &self,
        _req: &PaymentsSyncRouterData,
        connectors: &Connectors,
    ) -> CustomResult<String, errors::ConnectorError> {
        Ok(format!(
            "{}rest/getOrderStatusExtended.do",
            self.base_url(connectors)
        ))
    }

    fn get_request_body(
        &self,
        req: &PaymentsSyncRouterData,
        _connectors: &Connectors,
    ) -> CustomResult<RequestContent, errors::ConnectorError> {
        let connector_req = atbbank::AtbbankSyncRequest::try_from(req)?;
        Ok(RequestContent::FormUrlEncoded(Box::new(connector_req)))
    }

    fn build_request(
        &self,
        req: &PaymentsSyncRouterData,
        connectors: &Connectors,
    ) -> CustomResult<Option<Request>, errors::ConnectorError> {
        Ok(Some(
            RequestBuilder::new()
                .method(Method::Post)
                .url(&types::PaymentsSyncType::get_url(self, req, connectors)?)
                .attach_default_headers()
                .headers(types::PaymentsSyncType::get_headers(
                    self, req, connectors,
                )?)
                .set_body(types::PaymentsSyncType::get_request_body(
                    self, req, connectors,
                )?)
                .build(),
        ))
    }

    fn handle_response(
        &self,
        data: &PaymentsSyncRouterData,
        event_builder: Option<&mut ConnectorEvent>,
        res: Response,
    ) -> CustomResult<PaymentsSyncRouterData, errors::ConnectorError> {
        let response: atbbank::AtbbankSyncResponse = res
            .response
            .parse_struct("AtbbankSyncResponse")
            .change_context(errors::ConnectorError::ResponseDeserializationFailed)?;

        event_builder.map(|i| i.set_response_body(&response));
        router_env::logger::info!(connector_response=?response);

        RouterData::try_from(ResponseRouterData {
            response,
            data: data.clone(),
            http_code: res.status_code,
        })
    }

    fn get_error_response(
        &self,
        res: Response,
        event_builder: Option<&mut ConnectorEvent>,
    ) -> CustomResult<ErrorResponse, errors::ConnectorError> {
        self.build_error_response(res, event_builder)
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// PaymentCapture: POST /rest/deposit.do
// Captures a previously pre-authorized payment
// ──────────────────────────────────────────────────────────────────────────────

impl ConnectorIntegration<Capture, PaymentsCaptureData, PaymentsResponseData> for Atbbank {
    fn get_headers(
        &self,
        req: &PaymentsCaptureRouterData,
        connectors: &Connectors,
    ) -> CustomResult<Vec<(String, masking::Maskable<String>)>, errors::ConnectorError> {
        self.build_headers(req, connectors)
    }

    fn get_content_type(&self) -> &'static str {
        self.common_get_content_type()
    }

    fn get_url(
        &self,
        _req: &PaymentsCaptureRouterData,
        connectors: &Connectors,
    ) -> CustomResult<String, errors::ConnectorError> {
        Ok(format!("{}rest/deposit.do", self.base_url(connectors)))
    }

    fn get_request_body(
        &self,
        req: &PaymentsCaptureRouterData,
        _connectors: &Connectors,
    ) -> CustomResult<RequestContent, errors::ConnectorError> {
        let amount = utils::convert_amount(
            self.amount_converter,
            req.request.minor_amount_to_capture,
            req.request.currency,
        )?;

        let connector_req = atbbank::AtbbankCaptureRequest::try_from((req, amount))?;
        Ok(RequestContent::FormUrlEncoded(Box::new(connector_req)))
    }

    fn build_request(
        &self,
        req: &PaymentsCaptureRouterData,
        connectors: &Connectors,
    ) -> CustomResult<Option<Request>, errors::ConnectorError> {
        Ok(Some(
            RequestBuilder::new()
                .method(Method::Post)
                .url(&types::PaymentsCaptureType::get_url(self, req, connectors)?)
                .attach_default_headers()
                .headers(types::PaymentsCaptureType::get_headers(
                    self, req, connectors,
                )?)
                .set_body(types::PaymentsCaptureType::get_request_body(
                    self, req, connectors,
                )?)
                .build(),
        ))
    }

    fn handle_response(
        &self,
        data: &PaymentsCaptureRouterData,
        event_builder: Option<&mut ConnectorEvent>,
        res: Response,
    ) -> CustomResult<PaymentsCaptureRouterData, errors::ConnectorError> {
        let response: atbbank::AtbbankActionResponse = res
            .response
            .parse_struct("AtbbankCaptureResponse")
            .change_context(errors::ConnectorError::ResponseDeserializationFailed)?;

        event_builder.map(|i| i.set_response_body(&response));
        router_env::logger::info!(connector_response=?response);

        RouterData::try_from(ResponseRouterData {
            response,
            data: data.clone(),
            http_code: res.status_code,
        })
    }

    fn get_error_response(
        &self,
        res: Response,
        event_builder: Option<&mut ConnectorEvent>,
    ) -> CustomResult<ErrorResponse, errors::ConnectorError> {
        self.build_error_response(res, event_builder)
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// PaymentVoid: POST /rest/reverse.do
// Reverses (voids) a payment that has not yet been settled
// ──────────────────────────────────────────────────────────────────────────────

impl ConnectorIntegration<Void, PaymentsCancelData, PaymentsResponseData> for Atbbank {
    fn get_headers(
        &self,
        req: &PaymentsCancelRouterData,
        connectors: &Connectors,
    ) -> CustomResult<Vec<(String, masking::Maskable<String>)>, errors::ConnectorError> {
        self.build_headers(req, connectors)
    }

    fn get_content_type(&self) -> &'static str {
        self.common_get_content_type()
    }

    fn get_url(
        &self,
        _req: &PaymentsCancelRouterData,
        connectors: &Connectors,
    ) -> CustomResult<String, errors::ConnectorError> {
        Ok(format!("{}rest/reverse.do", self.base_url(connectors)))
    }

    fn get_request_body(
        &self,
        req: &PaymentsCancelRouterData,
        _connectors: &Connectors,
    ) -> CustomResult<RequestContent, errors::ConnectorError> {
        let connector_req = atbbank::AtbbankVoidRequest::try_from(req)?;
        Ok(RequestContent::FormUrlEncoded(Box::new(connector_req)))
    }

    fn build_request(
        &self,
        req: &PaymentsCancelRouterData,
        connectors: &Connectors,
    ) -> CustomResult<Option<Request>, errors::ConnectorError> {
        Ok(Some(
            RequestBuilder::new()
                .method(Method::Post)
                .url(&types::PaymentsVoidType::get_url(self, req, connectors)?)
                .attach_default_headers()
                .headers(types::PaymentsVoidType::get_headers(
                    self, req, connectors,
                )?)
                .set_body(types::PaymentsVoidType::get_request_body(
                    self, req, connectors,
                )?)
                .build(),
        ))
    }

    fn handle_response(
        &self,
        data: &PaymentsCancelRouterData,
        event_builder: Option<&mut ConnectorEvent>,
        res: Response,
    ) -> CustomResult<PaymentsCancelRouterData, errors::ConnectorError> {
        let response: atbbank::AtbbankActionResponse = res
            .response
            .parse_struct("AtbbankVoidResponse")
            .change_context(errors::ConnectorError::ResponseDeserializationFailed)?;

        event_builder.map(|i| i.set_response_body(&response));
        router_env::logger::info!(connector_response=?response);

        RouterData::try_from(ResponseRouterData {
            response,
            data: data.clone(),
            http_code: res.status_code,
        })
    }

    fn get_error_response(
        &self,
        res: Response,
        event_builder: Option<&mut ConnectorEvent>,
    ) -> CustomResult<ErrorResponse, errors::ConnectorError> {
        self.build_error_response(res, event_builder)
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Refund Execute: POST /rest/refund.do
// ──────────────────────────────────────────────────────────────────────────────

impl ConnectorIntegration<Execute, RefundsData, RefundsResponseData> for Atbbank {
    fn get_headers(
        &self,
        req: &RefundsRouterData<Execute>,
        connectors: &Connectors,
    ) -> CustomResult<Vec<(String, masking::Maskable<String>)>, errors::ConnectorError> {
        self.build_headers(req, connectors)
    }

    fn get_content_type(&self) -> &'static str {
        self.common_get_content_type()
    }

    fn get_url(
        &self,
        _req: &RefundsRouterData<Execute>,
        connectors: &Connectors,
    ) -> CustomResult<String, errors::ConnectorError> {
        Ok(format!("{}rest/refund.do", self.base_url(connectors)))
    }

    fn get_request_body(
        &self,
        req: &RefundsRouterData<Execute>,
        _connectors: &Connectors,
    ) -> CustomResult<RequestContent, errors::ConnectorError> {
        let refund_amount = utils::convert_amount(
            self.amount_converter,
            req.request.minor_refund_amount,
            req.request.currency,
        )?;

        let connector_router_data = atbbank::AtbbankRouterData::from((refund_amount, req));
        let connector_req = atbbank::AtbbankRefundRequest::try_from(&connector_router_data)?;
        Ok(RequestContent::FormUrlEncoded(Box::new(connector_req)))
    }

    fn build_request(
        &self,
        req: &RefundsRouterData<Execute>,
        connectors: &Connectors,
    ) -> CustomResult<Option<Request>, errors::ConnectorError> {
        let request = RequestBuilder::new()
            .method(Method::Post)
            .url(&types::RefundExecuteType::get_url(self, req, connectors)?)
            .attach_default_headers()
            .headers(types::RefundExecuteType::get_headers(
                self, req, connectors,
            )?)
            .set_body(types::RefundExecuteType::get_request_body(
                self, req, connectors,
            )?)
            .build();
        Ok(Some(request))
    }

    fn handle_response(
        &self,
        data: &RefundsRouterData<Execute>,
        event_builder: Option<&mut ConnectorEvent>,
        res: Response,
    ) -> CustomResult<RefundsRouterData<Execute>, errors::ConnectorError> {
        let response: atbbank::RefundResponse = res
            .response
            .parse_struct("AtbbankRefundResponse")
            .change_context(errors::ConnectorError::ResponseDeserializationFailed)?;

        event_builder.map(|i| i.set_response_body(&response));
        router_env::logger::info!(connector_response=?response);

        RouterData::try_from(ResponseRouterData {
            response,
            data: data.clone(),
            http_code: res.status_code,
        })
    }

    fn get_error_response(
        &self,
        res: Response,
        event_builder: Option<&mut ConnectorEvent>,
    ) -> CustomResult<ErrorResponse, errors::ConnectorError> {
        self.build_error_response(res, event_builder)
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// RefundSync: POST /rest/getOrderStatusExtended.do (same as PaymentSync)
// Checks order status and inspects refunded amounts
// ──────────────────────────────────────────────────────────────────────────────

impl ConnectorIntegration<RSync, RefundsData, RefundsResponseData> for Atbbank {
    fn get_headers(
        &self,
        req: &RefundSyncRouterData,
        connectors: &Connectors,
    ) -> CustomResult<Vec<(String, masking::Maskable<String>)>, errors::ConnectorError> {
        self.build_headers(req, connectors)
    }

    fn get_content_type(&self) -> &'static str {
        self.common_get_content_type()
    }

    fn get_url(
        &self,
        _req: &RefundSyncRouterData,
        connectors: &Connectors,
    ) -> CustomResult<String, errors::ConnectorError> {
        Ok(format!(
            "{}rest/getOrderStatusExtended.do",
            self.base_url(connectors)
        ))
    }

    fn get_request_body(
        &self,
        req: &RefundSyncRouterData,
        _connectors: &Connectors,
    ) -> CustomResult<RequestContent, errors::ConnectorError> {
        let connector_req = atbbank::AtbbankRefundSyncRequest::try_from(req)?;
        Ok(RequestContent::FormUrlEncoded(Box::new(connector_req)))
    }

    fn build_request(
        &self,
        req: &RefundSyncRouterData,
        connectors: &Connectors,
    ) -> CustomResult<Option<Request>, errors::ConnectorError> {
        Ok(Some(
            RequestBuilder::new()
                .method(Method::Post)
                .url(&types::RefundSyncType::get_url(self, req, connectors)?)
                .attach_default_headers()
                .headers(types::RefundSyncType::get_headers(
                    self, req, connectors,
                )?)
                .set_body(types::RefundSyncType::get_request_body(
                    self, req, connectors,
                )?)
                .build(),
        ))
    }

    fn handle_response(
        &self,
        data: &RefundSyncRouterData,
        event_builder: Option<&mut ConnectorEvent>,
        res: Response,
    ) -> CustomResult<RefundSyncRouterData, errors::ConnectorError> {
        let response: atbbank::AtbbankSyncResponse = res
            .response
            .parse_struct("AtbbankRefundSyncResponse")
            .change_context(errors::ConnectorError::ResponseDeserializationFailed)?;

        event_builder.map(|i| i.set_response_body(&response));
        router_env::logger::info!(connector_response=?response);

        RouterData::try_from(ResponseRouterData {
            response,
            data: data.clone(),
            http_code: res.status_code,
        })
    }

    fn get_error_response(
        &self,
        res: Response,
        event_builder: Option<&mut ConnectorEvent>,
    ) -> CustomResult<ErrorResponse, errors::ConnectorError> {
        self.build_error_response(res, event_builder)
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Webhooks: Not implemented for ATB SmartVista EPG
// ──────────────────────────────────────────────────────────────────────────────

#[async_trait::async_trait]
impl IncomingWebhook for Atbbank {
    fn get_webhook_object_reference_id(
        &self,
        _request: &IncomingWebhookRequestDetails<'_>,
    ) -> CustomResult<api_models::webhooks::ObjectReferenceId, errors::ConnectorError> {
        Err(report!(errors::ConnectorError::WebhooksNotImplemented))
    }

    fn get_webhook_event_type(
        &self,
        _request: &IncomingWebhookRequestDetails<'_>,
        _context: Option<&WebhookContext>,
    ) -> CustomResult<api_models::webhooks::IncomingWebhookEvent, errors::ConnectorError> {
        Err(report!(errors::ConnectorError::WebhooksNotImplemented))
    }

    fn get_webhook_resource_object(
        &self,
        _request: &IncomingWebhookRequestDetails<'_>,
    ) -> CustomResult<Box<dyn masking::ErasedMaskSerialize>, errors::ConnectorError> {
        Err(report!(errors::ConnectorError::WebhooksNotImplemented))
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Connector metadata: supported payment methods, info, webhook flows
// ──────────────────────────────────────────────────────────────────────────────

static ATBBANK_SUPPORTED_PAYMENT_METHODS: LazyLock<SupportedPaymentMethods> = LazyLock::new(|| {
    let supported_capture_methods = vec![
        enums::CaptureMethod::Automatic,
        enums::CaptureMethod::Manual,
    ];

    let supported_card_networks = vec![
        common_enums::CardNetwork::Visa,
        common_enums::CardNetwork::Mastercard,
    ];

    let mut supported = SupportedPaymentMethods::new();

    supported.add(
        enums::PaymentMethod::Card,
        enums::PaymentMethodType::Credit,
        PaymentMethodDetails {
            mandates: enums::FeatureStatus::NotSupported,
            refunds: enums::FeatureStatus::Supported,
            supported_capture_methods: supported_capture_methods.clone(),
            specific_features: Some(
                api_models::feature_matrix::PaymentMethodSpecificFeatures::Card({
                    api_models::feature_matrix::CardSpecificFeatures {
                        three_ds: common_enums::FeatureStatus::Supported,
                        no_three_ds: common_enums::FeatureStatus::Supported,
                        supported_card_networks: supported_card_networks.clone(),
                    }
                }),
            ),
        },
    );

    supported.add(
        enums::PaymentMethod::Card,
        enums::PaymentMethodType::Debit,
        PaymentMethodDetails {
            mandates: enums::FeatureStatus::NotSupported,
            refunds: enums::FeatureStatus::Supported,
            supported_capture_methods: supported_capture_methods.clone(),
            specific_features: Some(
                api_models::feature_matrix::PaymentMethodSpecificFeatures::Card({
                    api_models::feature_matrix::CardSpecificFeatures {
                        three_ds: common_enums::FeatureStatus::Supported,
                        no_three_ds: common_enums::FeatureStatus::Supported,
                        supported_card_networks: supported_card_networks.clone(),
                    }
                }),
            ),
        },
    );

    supported
});

static ATBBANK_CONNECTOR_INFO: ConnectorInfo = ConnectorInfo {
    display_name: "ATB Bank",
    description: "AzerTurkBank SmartVista EPG - direct bank card processing gateway supporting Visa, Mastercard, 3DS2, Apple Pay, and Google Pay for AZN/USD/EUR transactions.",
    connector_type: enums::HyperswitchConnectorCategory::PaymentGateway,
    integration_status: enums::ConnectorIntegrationStatus::Live,
};

static ATBBANK_SUPPORTED_WEBHOOK_FLOWS: [enums::EventClass; 0] = [];

impl api::ConnectorAccessTokenSuffix for Atbbank {}

impl ConnectorSpecifications for Atbbank {
    fn get_connector_about(&self) -> Option<&'static ConnectorInfo> {
        Some(&ATBBANK_CONNECTOR_INFO)
    }

    fn get_supported_payment_methods(&self) -> Option<&'static SupportedPaymentMethods> {
        Some(&*ATBBANK_SUPPORTED_PAYMENT_METHODS)
    }

    fn get_supported_webhook_flows(&self) -> Option<&'static [enums::EventClass]> {
        Some(&ATBBANK_SUPPORTED_WEBHOOK_FLOWS)
    }
}
