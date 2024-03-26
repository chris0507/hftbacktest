use std::{
    collections::HashMap,
    fmt::{Debug, Write},
};

use chrono::Utc;
use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use sha2::Sha256;
use thiserror::Error;

/// https://binance-docs.github.io/apidocs/futures/en/
use super::msg::{rest, rest::PositionInformationV2};
use crate::{
    connector::binancefutures::{
        msg::{
            rest::{OrderResponse, OrderResponseResult},
            stream::ListenKey,
        },
        ordermanager::{OrderManager, OrderMgr},
    },
    live::AssetInfo,
    ty::{AsStr, Error, ErrorType, OrdType, Order, Side, Status, TimeInForce},
};

#[derive(Error, Debug)]
pub enum RequestError {
    #[error("invalid request")]
    InvalidRequest,
    #[error("http error")]
    ReqError(#[from] reqwest::Error),
    #[error("order error")]
    OrderError(i64, String),
}

#[derive(Clone)]
pub struct BinanceFuturesClient {
    client: reqwest::Client,
    url: String,
    api_key: String,
    secret: String,
    orders: OrderMgr,
}

impl BinanceFuturesClient {
    pub fn new(url: &str, api_key: &str, secret: &str, orders: OrderMgr) -> Self {
        Self {
            client: reqwest::Client::new(),
            url: url.to_string(),
            api_key: api_key.to_string(),
            secret: secret.to_string(),
            orders,
        }
    }

    fn sign(secret: &str, s: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(s.as_bytes());
        let hash = mac.finalize().into_bytes();
        let mut tmp = String::with_capacity(hash.len() * 2);
        for c in hash {
            write!(&mut tmp, "{:02x}", c).unwrap();
        }
        tmp
    }

    async fn get<T: for<'a> Deserialize<'a>>(
        &self,
        path: &str,
        mut query: String,
        api_key: &str,
        secret: &str,
    ) -> Result<T, reqwest::Error> {
        let time = Utc::now().timestamp_millis() - 1000;
        if !query.is_empty() {
            query.push_str("&");
        }
        query.push_str("recvWindow=5000&timestamp=");
        query.push_str(&time.to_string());
        let signature = Self::sign(secret, &query);
        let resp = self
            .client
            .get(&format!(
                "{}{}?{}&signature={}",
                self.url, path, query, signature
            ))
            .header("Accept", "application/json")
            .header("X-MBX-APIKEY", api_key)
            .send()
            .await?
            .json()
            .await?;
        Ok(resp)
    }

    async fn put<T: for<'a> Deserialize<'a>>(
        &self,
        path: &str,
        body: String,
        api_key: &str,
        secret: &str,
    ) -> Result<T, reqwest::Error> {
        let time = Utc::now().timestamp_millis() - 1000;
        let sign_body = format!("recvWindow=5000&timestamp={}{}", time, body);
        let signature = Self::sign(secret, &sign_body);
        let resp = self
            .client
            .put(&format!(
                "{}{}?recvWindow=5000&timestamp={}&signature={}",
                self.url, path, time, signature
            ))
            .header("Accept", "application/json")
            .header("X-MBX-APIKEY", api_key)
            .body(body)
            .send()
            .await?
            .json()
            .await?;
        Ok(resp)
    }

    async fn post<T: for<'a> Deserialize<'a>>(
        &self,
        path: &str,
        body: String,
        api_key: &str,
        secret: &str,
    ) -> Result<T, reqwest::Error> {
        let time = Utc::now().timestamp_millis() - 1000;
        let sign_body = format!("recvWindow=5000&timestamp={}{}", time, body);
        let signature = Self::sign(secret, &sign_body);
        let resp = self
            .client
            .post(&format!(
                "{}{}?recvWindow=5000&timestamp={}&signature={}",
                self.url, path, time, signature
            ))
            .header("Accept", "application/json")
            .header("X-MBX-APIKEY", api_key)
            .body(body)
            .send()
            .await?
            .json()
            .await?;
        Ok(resp)
    }

    async fn delete<T: for<'a> Deserialize<'a>>(
        &self,
        path: &str,
        body: String,
        api_key: &str,
        secret: &str,
    ) -> Result<T, reqwest::Error> {
        let time = Utc::now().timestamp_millis() - 1000;
        let sign_body = format!("recvWindow=5000&timestamp={}{}", time, body);
        let signature = Self::sign(secret, &sign_body);
        let resp = self
            .client
            .delete(&format!(
                "{}{}?recvWindow=5000&timestamp={}&signature={}",
                self.url, path, time, signature
            ))
            .header("Accept", "application/json")
            .header("X-MBX-APIKEY", api_key)
            .body(body)
            .send()
            .await?
            .json()
            .await?;
        Ok(resp)
    }

    pub async fn start_user_data_stream(&self) -> Result<String, reqwest::Error> {
        let resp: Result<ListenKey, _> = self
            .post(
                "/fapi/v1/listenKey",
                String::new(),
                &self.api_key,
                &self.secret,
            )
            .await;
        resp.map(|v| v.listen_key)
    }

    pub async fn keepalive_user_data_stream(&self) -> Result<(), reqwest::Error> {
        let _: serde_json::Value = self
            .put(
                "/fapi/v1/listenKey",
                String::new(),
                &self.api_key,
                &self.secret,
            )
            .await?;
        Ok(())
    }

    pub async fn submit_order(
        &self,
        client_order_id: &str,
        symbol: &str,
        side: Side,
        price: f32,
        price_prec: usize,
        qty: f32,
        order_type: OrdType,
        time_in_force: TimeInForce,
    ) -> Result<OrderResponse, RequestError> {
        let mut body = String::with_capacity(200);
        body.push_str("newClientOrderId=");
        body.push_str(&client_order_id);
        body.push_str("&symbol=");
        body.push_str(&symbol);
        body.push_str("&side=");
        body.push_str(side.as_str());
        body.push_str("&price=");
        body.push_str(&format!("{:.prec$}", price, prec = price_prec));
        body.push_str("&quantity=");
        body.push_str(&format!("{:.5}", qty));
        body.push_str("&type=");
        body.push_str(order_type.as_str());
        body.push_str("&timeInForce=");
        body.push_str(time_in_force.as_str());

        let resp: OrderResponseResult = self
            .post("/fapi/v1/order", body, &self.api_key, &self.secret)
            .await?;
        match resp {
            OrderResponseResult::Ok(resp) => Ok(resp),
            OrderResponseResult::Err(resp) => Err(RequestError::OrderError(resp.code, resp.msg)),
        }
    }

    pub async fn submit_orders(
        &self,
        orders: Vec<(String, String, Side, f32, usize, f32, OrdType, TimeInForce)>,
    ) -> Result<Vec<Result<OrderResponse, RequestError>>, RequestError> {
        if orders.len() > 5 {
            return Err(RequestError::InvalidRequest);
        }
        let mut body = String::with_capacity(2000 * orders.len());
        body.push_str("{\"batchOrders\":[");
        for (i, order) in orders.iter().enumerate() {
            if i > 0 {
                body.push_str(",");
            }
            body.push_str("{\"newClientOrderId\":\"");
            body.push_str(&order.0);
            body.push_str("\",\"symbol\":\"");
            body.push_str(&order.1);
            body.push_str("\",\"side\":\"");
            body.push_str(order.2.as_str());
            body.push_str("\",\"price\":\"");
            body.push_str(&format!("{:.prec$}", order.3, prec = order.4));
            body.push_str("\",\"quantity\":\"");
            body.push_str(&format!("{:.5}", order.5));
            body.push_str("\",\"type\":\"");
            body.push_str(order.6.as_str());
            body.push_str("\",\"timeInForce\":\"");
            body.push_str(order.7.as_str());
            body.push_str("\"}");
        }
        body.push_str("]}");

        let resp: Vec<OrderResponseResult> = self
            .post("/fapi/v1/batchOrders", body, &self.api_key, &self.secret)
            .await?;
        Ok(resp
            .into_iter()
            .map(|resp| match resp {
                OrderResponseResult::Ok(resp) => Ok(resp),
                OrderResponseResult::Err(resp) => {
                    Err(RequestError::OrderError(resp.code, resp.msg))
                }
            })
            .collect())
    }

    pub async fn modify_order(
        &self,
        client_order_id: &str,
        symbol: &str,
        side: Side,
        price: f32,
        price_prec: usize,
        qty: f32,
    ) -> Result<OrderResponse, RequestError> {
        let mut body = String::with_capacity(100);
        body.push_str("symbol=");
        body.push_str(&symbol);
        body.push_str("&origClientOrderId=");
        body.push_str(&client_order_id);
        body.push_str("&side=");
        body.push_str(side.as_str());
        body.push_str("&price=");
        body.push_str(&format!("{:.prec$}", price, prec = price_prec));
        body.push_str("&quantity=");
        body.push_str(&format!("{:.5}", qty));

        let resp: OrderResponseResult = self
            .put("/fapi/v1/order", body, &self.api_key, &self.secret)
            .await?;
        match resp {
            OrderResponseResult::Ok(resp) => Ok(resp),
            OrderResponseResult::Err(resp) => Err(RequestError::OrderError(resp.code, resp.msg)),
        }
    }

    pub async fn cancel_order(
        &self,
        client_order_id: &str,
        symbol: &str,
    ) -> Result<OrderResponse, RequestError> {
        let mut body = String::with_capacity(100);
        body.push_str("symbol=");
        body.push_str(&symbol);
        body.push_str("&origClientOrderId=");
        body.push_str(client_order_id);

        let resp: OrderResponseResult = self
            .delete("/fapi/v1/order", body, &self.api_key, &self.secret)
            .await?;
        match resp {
            OrderResponseResult::Ok(resp) => Ok(resp),
            OrderResponseResult::Err(resp) => Err(RequestError::OrderError(resp.code, resp.msg)),
        }
    }

    pub async fn cancel_orders(
        &self,
        symbol: &str,
        client_order_ids: Vec<String>,
    ) -> Result<Vec<Result<OrderResponse, RequestError>>, RequestError> {
        if client_order_ids.len() > 10 {
            return Err(RequestError::InvalidRequest);
        }
        let mut body = String::with_capacity(100);
        body.push_str("{\"symbol\":\"");
        body.push_str(symbol);
        body.push_str("\",\"origClientOrderIdList\":[");
        for (i, client_order_id) in client_order_ids.iter().enumerate() {
            if i > 0 {
                body.push_str(",");
            }
            body.push_str("\"");
            body.push_str(client_order_id);
            body.push_str("\"");
        }
        body.push_str("]}");
        let resp: Vec<OrderResponseResult> = self
            .post("/fapi/v1/batchOrders", body, &self.api_key, &self.secret)
            .await?;
        Ok(resp
            .into_iter()
            .map(|resp| match resp {
                OrderResponseResult::Ok(resp) => Ok(resp),
                OrderResponseResult::Err(resp) => {
                    Err(RequestError::OrderError(resp.code, resp.msg))
                }
            })
            .collect())
    }

    pub async fn cancel_all_orders(&self, symbol: &str) -> Result<(), reqwest::Error> {
        let _: serde_json::Value = self
            .delete(
                "/fapi/v1/allOpenOrders",
                format!("symbol={}", symbol),
                &self.api_key,
                &self.secret,
            )
            .await?;
        Ok(())
    }

    pub async fn get_position_information(
        &self,
    ) -> Result<Vec<PositionInformationV2>, reqwest::Error> {
        let resp: Vec<PositionInformationV2> = self
            .get(
                "/fapi/v2/positionRisk",
                String::new(),
                &self.api_key,
                &self.secret,
            )
            .await?;
        Ok(resp)
    }

    pub async fn get_current_all_open_orders(
        &self,
        assets: &HashMap<String, AssetInfo>,
    ) -> Result<Vec<Order<()>>, reqwest::Error> {
        let resp: Vec<OrderResponse> = self
            .get(
                "/fapi/v1/openOrders",
                String::new(),
                &self.api_key,
                &self.secret,
            )
            .await?;
        Ok(resp
            .iter()
            .map(|data| {
                assets.get(&data.symbol).and_then(|asset_info| {
                    // fixme
                    OrderManager::parse_client_order_id(&data.client_order_id, "prefix").map(
                        |order_id| Order {
                            qty: data.orig_qty,
                            leaves_qty: data.orig_qty - data.cum_qty,
                            price_tick: (data.price / asset_info.tick_size).round() as i32,
                            tick_size: asset_info.tick_size,
                            side: data.side,
                            time_in_force: data.time_in_force,
                            exch_timestamp: data.update_time * 1_000_000,
                            status: data.status,
                            local_timestamp: 0,
                            req: Status::None,
                            exec_price_tick: 0,
                            exec_qty: data.executed_qty,
                            order_id,
                            order_type: data.type_,
                            // Invalid information
                            q: (),
                            // Invalid information
                            maker: false,
                        },
                    )
                })
            })
            .filter(|order| order.is_some())
            .map(|order| order.unwrap())
            .collect())
    }

    pub async fn get_depth(&self, symbol: &str) -> Result<rest::Depth, reqwest::Error> {
        let resp: rest::Depth = self
            .get(
                "/fapi/v1/depth",
                format!("symbol={}&limit=1000", symbol),
                &self.api_key,
                &self.secret,
            )
            .await?;
        Ok(resp)
    }
}
