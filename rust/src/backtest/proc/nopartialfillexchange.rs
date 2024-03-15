use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    mem,
    rc::Rc,
};

use crate::{
    backtest::{
        assettype::AssetType,
        models::{LatencyModel, QueueModel},
        order::OrderBus,
        proc::proc::Processor,
        reader::{
            Data,
            Reader,
            EXCH_ASK_DEPTH_CLEAR_EVENT,
            EXCH_ASK_DEPTH_EVENT,
            EXCH_ASK_DEPTH_SNAPSHOT_EVENT,
            EXCH_BID_DEPTH_CLEAR_EVENT,
            EXCH_BID_DEPTH_EVENT,
            EXCH_BID_DEPTH_SNAPSHOT_EVENT,
            EXCH_BUY_TRADE_EVENT,
            EXCH_EVENT,
            EXCH_SELL_TRADE_EVENT,
        },
        state::State,
        Error,
    },
    depth::{hashmapmarketdepth::HashMapMarketDepth, MarketDepth as _, INVALID_MAX, INVALID_MIN},
    ty::{Order, Row, Side, Status, TimeInForce, BUY, SELL},
};

pub struct NoPartialFillExchange<AT, Q, LM, QM>
where
    AT: AssetType,
    Q: Clone + Default,
    LM: LatencyModel,
    QM: QueueModel<Q>,
{
    reader: Reader<Row>,
    data: Data<Row>,
    row_num: usize,

    // key: order_id, value: Order<Q>
    orders: Rc<RefCell<HashMap<i64, Order<Q>>>>,
    // key: order's price tick, value: order_ids
    buy_orders: HashMap<i32, HashSet<i64>>,
    sell_orders: HashMap<i32, HashSet<i64>>,

    orders_to: OrderBus<Q>,
    orders_from: OrderBus<Q>,

    depth: HashMapMarketDepth,
    state: State<AT>,
    order_latency: LM,
    queue_model: QM,

    filled_orders: Vec<i64>,
}

impl<AT, Q, LM, QM> NoPartialFillExchange<AT, Q, LM, QM>
where
    AT: AssetType,
    Q: Clone + Default,
    LM: LatencyModel,
    QM: QueueModel<Q>,
{
    pub fn new(
        reader: Reader<Row>,
        depth: HashMapMarketDepth,
        state: State<AT>,
        order_latency: LM,
        queue_model: QM,
        orders_to: OrderBus<Q>,
        orders_from: OrderBus<Q>,
    ) -> Self {
        Self {
            reader,
            data: Data::empty(),
            row_num: 0,
            orders: Default::default(),
            buy_orders: Default::default(),
            sell_orders: Default::default(),
            orders_to,
            orders_from,
            depth,
            state,
            order_latency,
            queue_model,
            filled_orders: Default::default(),
        }
    }

    fn process_recv_order_(
        &mut self,
        mut order: Order<Q>,
        recv_timestamp: i64,
        wait_resp: i64,
        next_timestamp: i64,
    ) -> Result<i64, Error> {
        let order_id = order.order_id;

        // Processes a new order.
        if order.req == Status::New {
            order.req = Status::None;
            let resp_timestamp = self.ack_new(order, recv_timestamp)?;

            // Checks if the local waits for the orders' response.
            if wait_resp == order_id {
                // If next_timestamp is valid, chooses the earlier timestamp.
                return if next_timestamp > 0 {
                    Ok(next_timestamp.min(resp_timestamp))
                } else {
                    Ok(resp_timestamp)
                };
            }
        }
        // Processes a cancel order.
        else if order.req == Status::Canceled {
            order.req = Status::None;
            let resp_timestamp = self.ack_cancel(order, recv_timestamp)?;

            // Checks if the local waits for the orders' response.
            if wait_resp == order_id {
                // If next_timestamp is valid, chooses the earlier timestamp.
                return if next_timestamp > 0 {
                    Ok(next_timestamp.min(resp_timestamp))
                } else {
                    Ok(resp_timestamp)
                };
            }
        } else {
            return Err(Error::InvalidOrderRequest);
        }

        // Bypass next_timestamp
        Ok(next_timestamp)
    }

    fn check_if_sell_filled(
        &mut self,
        order: &mut Order<Q>,
        price_tick: i32,
        qty: f32,
        timestamp: i64,
    ) -> Result<i64, Error> {
        if order.price_tick < price_tick {
            self.filled_orders.push(order.order_id);
            return self.fill(order, timestamp, true, order.price_tick);
        } else if order.price_tick == price_tick {
            // Update the order's queue position.
            self.queue_model.trade(order, qty, &self.depth);
            if self.queue_model.is_filled(order, &self.depth) {
                self.filled_orders.push(order.order_id);
                return self.fill(order, timestamp, true, order.price_tick);
            }
        }
        Ok(i64::MAX)
    }

    fn check_if_buy_filled(
        &mut self,
        order: &mut Order<Q>,
        price_tick: i32,
        qty: f32,
        timestamp: i64,
    ) -> Result<i64, Error> {
        if order.price_tick > price_tick {
            self.filled_orders.push(order.order_id);
            return self.fill(order, timestamp, true, order.price_tick);
        } else if order.price_tick == price_tick {
            // Update the order's queue position.
            self.queue_model.trade(order, qty, &self.depth);
            if self.queue_model.is_filled(order, &self.depth) {
                self.filled_orders.push(order.order_id);
                return self.fill(order, timestamp, true, order.price_tick);
            }
        }
        Ok(i64::MAX)
    }

    fn fill(
        &mut self,
        order: &mut Order<Q>,
        timestamp: i64,
        maker: bool,
        exec_price_tick: i32,
    ) -> Result<i64, Error> {
        if order.status == Status::Expired
            || order.status == Status::Canceled
            || order.status == Status::Filled
        {
            return Err(Error::InvalidOrderStatus);
        }

        order.maker = maker;
        if maker {
            order.exec_price_tick = order.price_tick;
        } else {
            order.exec_price_tick = exec_price_tick;
        }

        order.exec_qty = order.leaves_qty;
        order.leaves_qty = 0.0;
        order.status = Status::Filled;
        order.exch_timestamp = timestamp;
        let local_recv_timestamp =
            order.exch_timestamp + self.order_latency.response(timestamp, &order);

        self.state.apply_fill(order);
        self.orders_to.append(order.clone(), local_recv_timestamp);
        Ok(local_recv_timestamp)
    }

    fn remove_filled_orders(&mut self) {
        if self.filled_orders.len() > 0 {
            let mut orders = self.orders.borrow_mut();
            for order_id in self.filled_orders.drain(..) {
                let order = orders.remove(&order_id).unwrap();
                if order.side == Side::Buy {
                    self.buy_orders
                        .get_mut(&order.price_tick)
                        .unwrap()
                        .remove(&order_id);
                } else {
                    self.sell_orders
                        .get_mut(&order.price_tick)
                        .unwrap()
                        .remove(&order_id);
                }
            }
        }
    }

    fn on_bid_qty_chg(&mut self, price_tick: i32, prev_qty: f32, new_qty: f32) {
        let orders = self.orders.clone();
        if let Some(order_ids) = self.buy_orders.get(&price_tick) {
            for order_id in order_ids.iter() {
                let mut orders_borrowed = orders.borrow_mut();
                let order = orders_borrowed.get_mut(order_id).unwrap();
                self.queue_model
                    .depth(order, prev_qty, new_qty, &self.depth);
            }
        }
    }

    fn on_ask_qty_chg(&mut self, price_tick: i32, prev_qty: f32, new_qty: f32) {
        let orders = self.orders.clone();
        if let Some(order_ids) = self.sell_orders.get(&price_tick) {
            for order_id in order_ids.iter() {
                let mut orders_borrowed = orders.borrow_mut();
                let order = orders_borrowed.get_mut(order_id).unwrap();
                self.queue_model
                    .depth(order, prev_qty, new_qty, &self.depth);
            }
        }
    }

    fn on_best_bid_update(
        &mut self,
        prev_best_tick: i32,
        new_best_tick: i32,
        timestamp: i64,
    ) -> Result<(), Error> {
        // If the best has been significantly updated compared to the previous best, it would be
        // better to iterate orders dict instead of order price ladder.
        {
            let orders = self.orders.clone();
            let mut orders_borrowed = orders.borrow_mut();
            if prev_best_tick == INVALID_MIN
                || (orders_borrowed.len() as i32) < new_best_tick - prev_best_tick
            {
                for (_, order) in orders_borrowed.iter_mut() {
                    if order.side == Side::Sell && order.price_tick <= new_best_tick {
                        self.filled_orders.push(order.order_id);
                        self.fill(order, timestamp, true, order.price_tick)?;
                    }
                }
            } else {
                for t in (prev_best_tick + 1)..=new_best_tick {
                    if let Some(order_ids) = self.sell_orders.get(&t) {
                        for order_id in order_ids.clone().iter() {
                            self.filled_orders.push(*order_id);
                            let order = orders_borrowed.get_mut(order_id).unwrap();
                            self.fill(order, timestamp, true, order.price_tick)?;
                        }
                    }
                }
            }
        }
        self.remove_filled_orders();
        Ok(())
    }

    fn on_best_ask_update(
        &mut self,
        prev_best_tick: i32,
        new_best_tick: i32,
        timestamp: i64,
    ) -> Result<(), Error> {
        // If the best has been significantly updated compared to the previous best, it would be
        // better to iterate orders dict instead of order price ladder.
        {
            let orders = self.orders.clone();
            let mut orders_borrowed = orders.borrow_mut();
            if prev_best_tick == INVALID_MAX
                || (orders_borrowed.len() as i32) < prev_best_tick - new_best_tick
            {
                for (_, order) in orders_borrowed.iter_mut() {
                    if order.side == Side::Buy && order.price_tick >= new_best_tick {
                        self.filled_orders.push(order.order_id);
                        self.fill(order, timestamp, true, order.price_tick)?;
                    }
                }
            } else {
                for t in new_best_tick..prev_best_tick {
                    if let Some(order_ids) = self.buy_orders.get(&t) {
                        for order_id in order_ids.clone().iter() {
                            self.filled_orders.push(*order_id);
                            let order = orders_borrowed.get_mut(order_id).unwrap();
                            self.fill(order, timestamp, true, order.price_tick)?;
                        }
                    }
                }
            }
        }
        self.remove_filled_orders();
        Ok(())
    }

    fn ack_new(&mut self, mut order: Order<Q>, timestamp: i64) -> Result<i64, Error> {
        if self.orders.borrow().contains_key(&order.order_id) {
            return Err(Error::OrderAlreadyExist);
        }

        if order.side == Side::Buy {
            // Checks if the buy order price is greater than or equal to the current best ask.
            if order.price_tick >= self.depth.best_ask_tick {
                if order.time_in_force == TimeInForce::GTX {
                    order.status = Status::Expired;

                    order.exch_timestamp = timestamp;
                    let local_recv_timestamp =
                        timestamp + self.order_latency.response(timestamp, &order);
                    self.orders_to.append(order.clone(), local_recv_timestamp);
                    Ok(local_recv_timestamp)
                } else {
                    // Takes the market.
                    self.fill(&mut order, timestamp, false, self.depth.best_ask_tick)
                }
            } else {
                // Initializes the order's queue position.
                self.queue_model.new_order(&mut order, &self.depth);
                order.status = Status::New;
                // The exchange accepts this order.
                self.buy_orders
                    .entry(order.price_tick)
                    .or_insert(HashSet::new())
                    .insert(order.order_id);

                order.exch_timestamp = timestamp;
                let local_recv_timestamp =
                    timestamp + self.order_latency.response(timestamp, &order);
                self.orders_to.append(order.clone(), local_recv_timestamp);

                self.orders.borrow_mut().insert(order.order_id, order);

                Ok(local_recv_timestamp)
            }
        } else {
            // Checks if the sell order price is less than or equal to the current best bid.
            if order.price_tick <= self.depth.best_bid_tick {
                if order.time_in_force == TimeInForce::GTX {
                    order.status = Status::Expired;

                    order.exch_timestamp = timestamp;
                    let local_recv_timestamp =
                        timestamp + self.order_latency.response(timestamp, &order);
                    self.orders_to.append(order.clone(), local_recv_timestamp);
                    Ok(local_recv_timestamp)
                } else {
                    // Takes the market.
                    self.fill(&mut order, timestamp, false, self.depth.best_bid_tick)
                }
            } else {
                // Initializes the order's queue position.
                self.queue_model.new_order(&mut order, &self.depth);
                order.status = Status::New;
                // The exchange accepts this order.
                self.sell_orders
                    .entry(order.price_tick)
                    .or_insert(HashSet::new())
                    .insert(order.order_id);

                order.exch_timestamp = timestamp;
                let local_recv_timestamp =
                    timestamp + self.order_latency.response(timestamp, &order);
                self.orders_to.append(order.clone(), local_recv_timestamp);

                self.orders.borrow_mut().insert(order.order_id, order);

                Ok(local_recv_timestamp)
            }
        }
    }

    fn ack_cancel(&mut self, mut order: Order<Q>, timestamp: i64) -> Result<i64, Error> {
        let exch_order = {
            let mut order_borrowed = self.orders.borrow_mut();
            order_borrowed.remove(&order.order_id)
        };

        if exch_order.is_none() {
            order.status = Status::Expired;
            order.exch_timestamp = timestamp;
            let local_recv_timestamp = timestamp + self.order_latency.response(timestamp, &order);
            // It can overwrite another existing order on the local side if order_id is the same.
            // So, commented out.
            // self.orders_to.append(order.copy(), local_recv_timestamp)
            return Ok(local_recv_timestamp);
        }

        // Delete the order.
        let mut exch_order = exch_order.unwrap();
        if exch_order.side == Side::Buy {
            self.buy_orders
                .get_mut(&exch_order.price_tick)
                .unwrap()
                .remove(&exch_order.order_id);
        } else {
            self.sell_orders
                .get_mut(&exch_order.price_tick)
                .unwrap()
                .remove(&exch_order.order_id);
        }

        // Make the response.
        exch_order.status = Status::Canceled;
        exch_order.exch_timestamp = timestamp;
        let local_recv_timestamp = timestamp + self.order_latency.response(timestamp, &exch_order);
        self.orders_to
            .append(exch_order.clone(), local_recv_timestamp);
        Ok(local_recv_timestamp)
    }

    fn ack_modify(&mut self, mut order: Order<Q>, timestamp: i64) -> Result<i64, Error> {
        let mut exch_order = {
            let mut order_borrowed = self.orders.borrow_mut();
            let exch_order = order_borrowed.remove(&order.order_id);

            // The order can be already deleted due to fill or expiration.
            if exch_order.is_none() {
                order.status = Status::Expired;
                order.exch_timestamp = timestamp;
                let local_recv_timestamp =
                    timestamp + self.order_latency.response(timestamp, &order);
                // It can overwrite another existing order on the local side if order_id is the
                // same. So, commented out.
                // self.orders_to.append(order.copy(), local_recv_timestamp)
                return Ok(local_recv_timestamp);
            }

            exch_order.unwrap()
        };

        let prev_price_tick = exch_order.price_tick;
        exch_order.price_tick = order.price_tick;
        // No partial fill occurs.
        exch_order.qty = order.qty;
        // The initialization of the order queue position may not occur when the modified quantity
        // is smaller than the previous quantity, depending on the exchanges. It may need to
        // implement exchange-specific specialization.
        let init_q_pos = true;

        if exch_order.side == Side::Buy {
            // Check if the buy order price is greater than or equal to the current best ask.
            if exch_order.price_tick >= self.depth.best_ask_tick {
                self.buy_orders
                    .get_mut(&prev_price_tick)
                    .unwrap()
                    .remove(&exch_order.order_id);

                if exch_order.time_in_force == TimeInForce::GTX {
                    exch_order.status = Status::Expired;
                } else {
                    // Take the market.
                    return self.fill(&mut exch_order, timestamp, false, self.depth.best_ask_tick);
                }

                exch_order.exch_timestamp = timestamp;
                let local_recv_timestamp =
                    timestamp + self.order_latency.response(timestamp, &exch_order);
                self.orders_to
                    .append(exch_order.clone(), local_recv_timestamp);
                Ok(local_recv_timestamp)
            } else {
                // The exchange accepts this order.
                if prev_price_tick != exch_order.price_tick {
                    self.buy_orders
                        .get_mut(&prev_price_tick)
                        .unwrap()
                        .remove(&exch_order.order_id);
                    self.buy_orders
                        .entry(exch_order.price_tick)
                        .or_insert(HashSet::new())
                        .insert(exch_order.order_id);
                }
                if init_q_pos || prev_price_tick != exch_order.price_tick {
                    // Initialize the order's queue position.
                    self.queue_model.new_order(&mut exch_order, &self.depth);
                }
                exch_order.status = Status::New;

                exch_order.exch_timestamp = timestamp;
                let local_recv_timestamp =
                    timestamp + self.order_latency.response(timestamp, &exch_order);
                self.orders_to
                    .append(exch_order.clone(), local_recv_timestamp);

                let mut order_borrowed = self.orders.borrow_mut();
                order_borrowed.insert(exch_order.order_id, exch_order);

                Ok(local_recv_timestamp)
            }
        } else {
            // Check if the sell order price is less than or equal to the current best bid.
            if exch_order.price_tick <= self.depth.best_bid_tick {
                self.sell_orders
                    .get_mut(&prev_price_tick)
                    .unwrap()
                    .remove(&exch_order.order_id);

                if exch_order.time_in_force == TimeInForce::GTX {
                    exch_order.status = Status::Expired;
                } else {
                    // Take the market.
                    return self.fill(&mut exch_order, timestamp, false, self.depth.best_bid_tick);
                }

                exch_order.exch_timestamp = timestamp;
                let local_recv_timestamp =
                    timestamp + self.order_latency.response(timestamp, &exch_order);
                self.orders_to
                    .append(exch_order.clone(), local_recv_timestamp);
                Ok(local_recv_timestamp)
            } else {
                // The exchange accepts this order.
                if prev_price_tick != exch_order.price_tick {
                    self.sell_orders
                        .get_mut(&prev_price_tick)
                        .unwrap()
                        .remove(&exch_order.order_id);
                    self.sell_orders
                        .entry(exch_order.price_tick)
                        .or_insert(HashSet::new())
                        .insert(exch_order.order_id);
                }
                if init_q_pos || prev_price_tick != exch_order.price_tick {
                    // Initialize the order's queue position.
                    self.queue_model.new_order(&mut exch_order, &self.depth);
                }
                exch_order.status = Status::New;

                exch_order.exch_timestamp = timestamp;
                let local_recv_timestamp =
                    timestamp + self.order_latency.response(timestamp, &exch_order);
                self.orders_to
                    .append(exch_order.clone(), local_recv_timestamp);

                let mut order_borrowed = self.orders.borrow_mut();
                order_borrowed.insert(exch_order.order_id, exch_order);

                Ok(local_recv_timestamp)
            }
        }
    }
}

impl<AT, Q, LM, QM> Processor for NoPartialFillExchange<AT, Q, LM, QM>
where
    Q: Clone + Default,
    AT: AssetType,
    LM: LatencyModel,
    QM: QueueModel<Q>,
{
    fn initialize_data(&mut self) -> Result<i64, Error> {
        self.data = self.reader.next()?;
        for rn in 0..self.data.len() {
            if self.data[rn].ev & EXCH_EVENT == EXCH_EVENT {
                self.row_num = rn;
                return Ok(self.data[rn].local_ts);
            }
        }
        Err(Error::EndOfData)
    }

    fn process_data(&mut self) -> Result<(i64, i64), Error> {
        let row_num = self.row_num;
        if self.data[row_num].ev & EXCH_BID_DEPTH_CLEAR_EVENT == EXCH_BID_DEPTH_CLEAR_EVENT {
            self.depth.clear_depth(BUY, self.data[row_num].px);
        } else if self.data[row_num].ev & EXCH_ASK_DEPTH_CLEAR_EVENT == EXCH_ASK_DEPTH_CLEAR_EVENT {
            self.depth.clear_depth(SELL, self.data[row_num].px);
        } else if self.data[row_num].ev & EXCH_BID_DEPTH_EVENT == EXCH_BID_DEPTH_EVENT
            || self.data[row_num].ev & EXCH_BID_DEPTH_SNAPSHOT_EVENT
                == EXCH_BID_DEPTH_SNAPSHOT_EVENT
        {
            let (price_tick, prev_best_bid_tick, best_bid_tick, prev_qty, new_qty, timestamp) =
                self.depth.update_bid_depth(
                    self.data[row_num].px,
                    self.data[row_num].qty,
                    self.data[row_num].exch_ts,
                );
            self.on_bid_qty_chg(price_tick, prev_qty, new_qty);
            if best_bid_tick > prev_best_bid_tick {
                self.on_best_bid_update(prev_best_bid_tick, best_bid_tick, timestamp)?;
            }
        } else if self.data[row_num].ev & EXCH_ASK_DEPTH_EVENT == EXCH_ASK_DEPTH_EVENT
            || self.data[row_num].ev & EXCH_ASK_DEPTH_SNAPSHOT_EVENT
                == EXCH_ASK_DEPTH_SNAPSHOT_EVENT
        {
            let (price_tick, prev_best_ask_tick, best_ask_tick, prev_qty, new_qty, timestamp) =
                self.depth.update_ask_depth(
                    self.data[row_num].px,
                    self.data[row_num].qty,
                    self.data[row_num].exch_ts,
                );
            self.on_ask_qty_chg(price_tick, prev_qty, new_qty);
            if best_ask_tick < prev_best_ask_tick {
                self.on_best_ask_update(prev_best_ask_tick, best_ask_tick, timestamp)?;
            }
        } else if self.data[row_num].ev & EXCH_BUY_TRADE_EVENT == EXCH_BUY_TRADE_EVENT {
            let price_tick = (self.data[row_num].px / self.depth.tick_size).round() as i32;
            let qty = self.data[row_num].qty;
            {
                let orders = self.orders.clone();
                let mut orders_borrowed = orders.borrow_mut();
                if self.depth.best_bid_tick == INVALID_MIN
                    || (orders_borrowed.len() as i32) < price_tick - self.depth.best_bid_tick
                {
                    for (_, order) in orders_borrowed.iter_mut() {
                        if order.side == Side::Sell {
                            self.check_if_sell_filled(
                                order,
                                price_tick,
                                qty,
                                self.data[row_num].exch_ts,
                            )?;
                        }
                    }
                } else {
                    for t in (self.depth.best_bid_tick + 1)..=price_tick {
                        if let Some(order_ids) = self.sell_orders.get(&t) {
                            for order_id in order_ids.clone().iter() {
                                let order = orders_borrowed.get_mut(&order_id).unwrap();
                                self.check_if_sell_filled(
                                    order,
                                    price_tick,
                                    qty,
                                    self.data[row_num].exch_ts,
                                )?;
                            }
                        }
                    }
                }
            }
            self.remove_filled_orders();
        } else if self.data[row_num].ev & EXCH_SELL_TRADE_EVENT == EXCH_SELL_TRADE_EVENT {
            let price_tick = (self.data[row_num].px / self.depth.tick_size).round() as i32;
            let qty = self.data[row_num].qty;
            {
                let orders = self.orders.clone();
                let mut orders_borrowed = orders.borrow_mut();
                if self.depth.best_ask_tick == INVALID_MAX
                    || (orders_borrowed.len() as i32) < self.depth.best_ask_tick - price_tick
                {
                    for (_, order) in orders_borrowed.iter_mut() {
                        if order.side == Side::Buy {
                            self.check_if_buy_filled(
                                order,
                                price_tick,
                                qty,
                                self.data[row_num].exch_ts,
                            )?;
                        }
                    }
                } else {
                    for t in (price_tick..self.depth.best_ask_tick).rev() {
                        if let Some(order_ids) = self.buy_orders.get(&t) {
                            for order_id in order_ids.clone().iter() {
                                let order = orders_borrowed.get_mut(&order_id).unwrap();
                                self.check_if_buy_filled(
                                    order,
                                    price_tick,
                                    qty,
                                    self.data[row_num].exch_ts,
                                )?;
                            }
                        }
                    }
                }
            }
            self.remove_filled_orders();
        }

        // Checks
        let mut next_ts = 0;
        for rn in (self.row_num + 1)..self.data.len() {
            if self.data[rn].ev & EXCH_EVENT == EXCH_EVENT {
                self.row_num = rn;
                next_ts = self.data[rn].exch_ts;
                break;
            }
        }

        if next_ts <= 0 {
            let next_data = self.reader.next()?;
            let next_row = &next_data[0];
            next_ts = next_row.exch_ts;
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
