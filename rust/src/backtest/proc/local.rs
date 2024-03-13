use std::{
    collections::{hash_map::Entry, HashMap},
    mem,
};

use crate::{
    backtest::{
        assettype::AssetType,
        models::LatencyModel,
        order::OrderBus,
        proc::proc::{LocalProcessor, Processor},
        reader::{
            Data,
            Reader,
            LOCAL_ASK_DEPTH_CLEAR_EVENT,
            LOCAL_ASK_DEPTH_EVENT,
            LOCAL_ASK_DEPTH_SNAPSHOT_EVENT,
            LOCAL_BID_DEPTH_CLEAR_EVENT,
            LOCAL_BID_DEPTH_EVENT,
            LOCAL_BID_DEPTH_SNAPSHOT_EVENT,
            LOCAL_EVENT,
            LOCAL_TRADE_EVENT,
        },
        state::{State, StateValues},
        Error,
    },
    depth::MarketDepth,
    ty::{OrdType, Order, Row, Side, Status, TimeInForce, BUY, SELL},
};

pub struct Local<AT, Q, LM, MD>
where
    AT: AssetType,
    Q: Clone,
    LM: LatencyModel,
    MD: MarketDepth,
{
    pub reader: Reader<Row>,
    pub data: Data<Row>,
    pub row_num: usize,
    pub orders: HashMap<i64, Order<Q>>,
    pub orders_to: OrderBus<Q>,
    pub orders_from: OrderBus<Q>,
    pub depth: MD,
    pub state: State<AT>,
    pub order_latency: LM,
    pub trades: Vec<Row>,
    pub last_order_entry_latency: Option<i64>,
    pub last_roundtrip_order_latency: Option<i64>,
}

impl<AT, Q, LM, MD> Local<AT, Q, LM, MD>
where
    AT: AssetType,
    Q: Clone + Default,
    LM: LatencyModel,
    MD: MarketDepth,
{
    pub fn new(
        reader: Reader<Row>,
        depth: MD,
        state: State<AT>,
        order_latency: LM,
        trade_len: usize,
        orders_to: OrderBus<Q>,
        orders_from: OrderBus<Q>,
    ) -> Self {
        Self {
            reader,
            data: Data::empty(),
            row_num: 0,
            orders: Default::default(),
            orders_to,
            orders_from,
            depth,
            state,
            order_latency,
            trades: Vec::with_capacity(trade_len),
            last_order_entry_latency: None,
            last_roundtrip_order_latency: None,
        }
    }

    fn process_recv_order_(
        &mut self,
        order: Order<Q>,
        _recv_timestamp: i64,
        _wait_resp: i64,
        next_timestamp: i64,
    ) -> Result<i64, Error> {
        if order.status == Status::Filled {
            self.state.apply_fill(&order);
        }
        // Applies the received order response to the local orders.
        match self.orders.entry(order.order_id) {
            Entry::Occupied(mut entry) => {
                *entry.get_mut() = order;
            }
            Entry::Vacant(entry) => {
                entry.insert(order);
            }
        }

        // Bypass next_timestamp
        Ok(next_timestamp)
    }

    pub fn clear_last_trades(&mut self) {
        self.trades.clear();
    }
}

impl<AT, Q, LM, MD> LocalProcessor<Q, MD> for Local<AT, Q, LM, MD>
where
    AT: AssetType,
    Q: Clone + Default,
    LM: LatencyModel,
    MD: MarketDepth,
{
    fn submit_order(
        &mut self,
        order_id: i64,
        side: Side,
        price: f32,
        qty: f32,
        order_type: OrdType,
        time_in_force: TimeInForce,
        current_timestamp: i64,
    ) -> Result<(), Error> {
        if self.orders.contains_key(&order_id) {
            return Err(Error::OrderAlreadyExist);
        }

        let price_tick = (price / self.depth.tick_size()).round() as i32;
        let mut order = Order::new(
            order_id,
            price_tick,
            self.depth.tick_size(),
            qty,
            side,
            order_type,
            time_in_force,
        );
        order.req = Status::New;
        let exch_recv_timestamp =
            current_timestamp + self.order_latency.entry(current_timestamp, &order);

        self.orders_to.append(order.clone(), exch_recv_timestamp);
        self.orders.insert(order.order_id, order);
        Ok(())
    }

    fn cancel(&mut self, order_id: i64, current_timestamp: i64) -> Result<(), Error> {
        let order = self.orders.get_mut(&order_id).ok_or(Error::OrderNotFound)?;

        if order.req != Status::None {
            return Err(Error::OrderRequestInProcess);
        }

        order.req = Status::Canceled;
        let exch_recv_timestamp =
            current_timestamp + self.order_latency.entry(current_timestamp, order);

        self.orders_to.append(order.clone(), exch_recv_timestamp);
        Ok(())
    }

    fn clear_inactive_orders(&mut self) {
        self.orders.retain(|_, order| {
            order.status != Status::Expired
                && order.status != Status::Filled
                && order.status != Status::Canceled
        })
    }

    fn position(&self) -> f64 {
        self.state.position
    }

    fn state_values(&self) -> StateValues {
        StateValues {
            position: self.state.position,
            balance: self.state.balance,
            fee: self.state.fee,
            trade_num: self.state.trade_num,
            trade_qty: self.state.trade_qty,
            trade_amount: self.state.trade_amount,
        }
    }

    fn depth(&self) -> &MD {
        &self.depth
    }

    fn orders(&self) -> &HashMap<i64, Order<Q>> {
        &self.orders
    }

    fn trade(&self) -> &Vec<Row> {
        &self.trades
    }

    fn clear_last_trades(&mut self) {
        self.trades.clear();
    }
}

impl<AT, Q, LM, MD> Processor for Local<AT, Q, LM, MD>
where
    AT: AssetType,
    Q: Clone + Default,
    LM: LatencyModel,
    MD: MarketDepth,
{
    fn initialize_data(&mut self) -> Result<i64, Error> {
        self.data = self.reader.next()?;
        for rn in 0..self.data.len() {
            if self.data[rn].ev & LOCAL_EVENT == LOCAL_EVENT {
                self.row_num = rn;
                return Ok(self.data[rn].local_ts);
            }
        }
        Err(Error::EndOfData)
    }

    fn process_data(&mut self) -> Result<(i64, i64), Error> {
        let row = &self.data[self.row_num];
        // Processes a depth event
        if row.ev & LOCAL_BID_DEPTH_CLEAR_EVENT == LOCAL_BID_DEPTH_CLEAR_EVENT {
            self.depth.clear_depth(BUY, row.px);
        } else if row.ev & LOCAL_ASK_DEPTH_CLEAR_EVENT == LOCAL_ASK_DEPTH_CLEAR_EVENT {
            self.depth.clear_depth(SELL, row.px);
        } else if row.ev & LOCAL_BID_DEPTH_EVENT == LOCAL_BID_DEPTH_EVENT
            || row.ev & LOCAL_BID_DEPTH_SNAPSHOT_EVENT == LOCAL_BID_DEPTH_SNAPSHOT_EVENT
        {
            self.depth.update_bid_depth(row.px, row.qty, row.local_ts);
        } else if row.ev & LOCAL_ASK_DEPTH_EVENT == LOCAL_ASK_DEPTH_EVENT
            || row.ev & LOCAL_ASK_DEPTH_SNAPSHOT_EVENT == LOCAL_ASK_DEPTH_SNAPSHOT_EVENT
        {
            self.depth.update_ask_depth(row.px, row.qty, row.local_ts);
        }
        // Processes a trade event
        else if row.ev & LOCAL_TRADE_EVENT == LOCAL_TRADE_EVENT {
            if self.trades.capacity() > 0 {
                self.trades.push(row.clone());
            }
        }

        // Checks
        let mut next_ts = 0;
        for rn in (self.row_num + 1)..self.data.len() {
            if self.data[rn].ev & LOCAL_EVENT == LOCAL_EVENT {
                self.row_num = rn;
                next_ts = self.data[rn].local_ts;
                break;
            }
        }

        if next_ts <= 0 {
            let next_data = self.reader.next()?;
            let next_row = &next_data[0];
            next_ts = next_row.local_ts;
            let data = mem::replace(&mut self.data, next_data);
            self.reader.release(data);
            self.row_num = 0;
        }
        Ok((next_ts, i64::MAX))
    }

    fn process_recv_order(&mut self, timestamp: i64, wait_resp: i64) -> Result<i64, Error> {
        // Processes the order part.
        let mut next_timestamp = i64::MAX;
        while self.orders_from.len() > 0 {
            let recv_timestamp = self.orders_from.get_head_timestamp().unwrap();
            if timestamp == recv_timestamp {
                let order = self.orders_from.remove(0);
                self.last_order_entry_latency = Some(order.exch_timestamp - order.local_timestamp);
                self.last_roundtrip_order_latency = Some(recv_timestamp - order.local_timestamp);
                next_timestamp =
                    self.process_recv_order_(order, recv_timestamp, wait_resp, next_timestamp)?;
            } else {
                assert!(recv_timestamp > timestamp);
                break;
            }
        }
        Ok(next_timestamp)
    }

    fn frontmost_recv_order_timestamp(&self) -> i64 {
        self.orders_from.frontmost_timestamp()
    }

    fn frontmost_send_order_timestamp(&self) -> i64 {
        self.orders_to.frontmost_timestamp()
    }
}
