//! An in-memory LND: invoices, decoding, payments, and the invoice stream.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::anyhow;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use tokio::sync::mpsc;

use crate::lnd::{
    AddedInvoice, BoxFuture, Chain, DecodedInvoice, InvoiceRequest, InvoiceStream, Lightning, LndInvoice, NodeBalances,
    NodeInfo, PaymentStatus, SendRequest,
};
use crate::util::{now, sha256};

#[derive(Debug, Clone)]
pub(crate) struct MockInvoice {
    pub(crate) bolt11: String,
    pub(crate) amount_msat: u64,
    pub(crate) memo: String,
    pub(crate) description_hash: Option<[u8; 32]>,
    pub(crate) state: &'static str,
    pub(crate) settle_index: u64,
}

#[derive(Debug)]
pub(crate) struct MockState {
    pub(crate) network: String,
    pub(crate) invoices: HashMap<String, MockInvoice>,
    /// Invoices of other nodes, by BOLT11 string.
    pub(crate) external: HashMap<String, DecodedInvoice>,
    /// `None` makes `send_payment` fail with an unknown outcome.
    pub(crate) send_result: Option<PaymentStatus>,
    pub(crate) track_result: PaymentStatus,
    pub(crate) sends: Vec<SendRequest>,
    pub(crate) canceled: Vec<String>,
    pub(crate) channel_local_msat: u64,
    counter: u64,
    settle_index: u64,
    stream: Option<mpsc::UnboundedReceiver<LndInvoice>>,
}

#[derive(Debug)]
pub(crate) struct MockLnd {
    state: Mutex<MockState>,
    pub(crate) stream: mpsc::UnboundedSender<LndInvoice>,
}

fn hash_for(label: &str) -> String {
    hex::encode(sha256(label.as_bytes()))
}

impl MockLnd {
    pub(crate) fn new(network: &str) -> Arc<Self> {
        let (sender, receiver) = mpsc::unbounded_channel();
        Arc::new(Self {
            state: Mutex::new(MockState {
                network: network.to_owned(),
                invoices: HashMap::new(),
                external: HashMap::new(),
                send_result: Some(PaymentStatus::Succeeded { fee_msat: 0 }),
                track_result: PaymentStatus::NotFound,
                sends: Vec::new(),
                canceled: Vec::new(),
                channel_local_msat: 1_000_000_000_000,
                counter: 0,
                settle_index: 0,
                stream: Some(receiver),
            }),
            stream: sender,
        })
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, MockState> {
        self.state.lock().unwrap()
    }

    /// Marks one of this node's invoices paid over Lightning and returns the update LND would stream.
    pub(crate) fn pay(&self, payment_hash: &str) -> LndInvoice {
        let mut state = self.lock();
        state.settle_index += 1;
        let settle_index = state.settle_index;
        let invoice = state.invoices.get_mut(payment_hash).expect("known invoice");
        invoice.state = "SETTLED";
        invoice.settle_index = settle_index;
        LndInvoice {
            r_hash: STANDARD.encode(hex::decode(payment_hash).unwrap()),
            state: "SETTLED".into(),
            amt_paid_msat: invoice.amount_msat,
            settle_index,
        }
    }

    /// An invoice from another node that `decode_invoice` understands.
    pub(crate) fn external_invoice(&self, amount_msat: u64, description_hash: Option<[u8; 32]>) -> (String, String) {
        let mut state = self.lock();
        state.counter += 1;
        let bolt11 = format!("lntbs{amount_msat}n1external{}", state.counter);
        let payment_hash = hash_for(&bolt11);
        state.external.insert(
            bolt11.clone(),
            DecodedInvoice {
                destination: "02".to_owned() + &"ab".repeat(32),
                payment_hash: payment_hash.clone(),
                num_msat: amount_msat,
                timestamp: u64::try_from(now()).unwrap(),
                expiry: 3600,
                description: "external".into(),
                description_hash: description_hash.map(hex::encode).unwrap_or_default(),
            },
        );
        (bolt11, payment_hash)
    }
}

struct MockStream(mpsc::UnboundedReceiver<LndInvoice>);

impl InvoiceStream for MockStream {
    fn next(&mut self) -> BoxFuture<'_, Option<LndInvoice>> {
        Box::pin(async move { Ok(self.0.recv().await) })
    }
}

impl Lightning for MockLnd {
    fn get_info(&self) -> BoxFuture<'_, NodeInfo> {
        let network = self.lock().network.clone();
        Box::pin(async move {
            Ok(NodeInfo {
                alias: "mock".into(),
                identity_pubkey: "02".to_owned() + &"cd".repeat(32),
                chains: vec![Chain {
                    chain: "bitcoin".into(),
                    network,
                }],
                synced_to_chain: true,
            })
        })
    }

    fn add_invoice(&self, request: InvoiceRequest) -> BoxFuture<'_, AddedInvoice> {
        let mut state = self.lock();
        state.counter += 1;
        let bolt11 = format!("lntbs{}n1mock{}", request.amount_msat, state.counter);
        let payment_hash = hash_for(&bolt11);
        state.invoices.insert(
            payment_hash.clone(),
            MockInvoice {
                bolt11: bolt11.clone(),
                amount_msat: request.amount_msat,
                memo: request.memo,
                description_hash: request.description_hash,
                state: "OPEN",
                settle_index: 0,
            },
        );
        Box::pin(async move { Ok(AddedInvoice { payment_hash, bolt11 }) })
    }

    fn lookup_invoice(&self, payment_hash: String) -> BoxFuture<'_, Option<LndInvoice>> {
        let found = self.lock().invoices.get(&payment_hash).map(|invoice| LndInvoice {
            r_hash: STANDARD.encode(hex::decode(payment_hash.as_str()).unwrap()),
            state: invoice.state.into(),
            amt_paid_msat: if invoice.state == "SETTLED" {
                invoice.amount_msat
            } else {
                0
            },
            settle_index: invoice.settle_index,
        });
        Box::pin(async move { Ok(found) })
    }

    fn cancel_invoice(&self, payment_hash: String) -> BoxFuture<'_, ()> {
        let mut state = self.lock();
        let result = match state.invoices.get_mut(&payment_hash) {
            Some(invoice) if invoice.state == "SETTLED" => Err(anyhow!("invoice already settled")),
            Some(invoice) => {
                invoice.state = "CANCELED";
                Ok(())
            }
            None => Err(anyhow!("unable to locate invoice")),
        };
        if result.is_ok() {
            state.canceled.push(payment_hash);
        }
        Box::pin(async move { result })
    }

    fn decode_invoice(&self, bolt11: String) -> BoxFuture<'_, DecodedInvoice> {
        let state = self.lock();
        let ours =
            state
                .invoices
                .iter()
                .find(|(_, invoice)| invoice.bolt11 == bolt11)
                .map(|(payment_hash, invoice)| DecodedInvoice {
                    destination: "02".to_owned() + &"cd".repeat(32),
                    payment_hash: payment_hash.clone(),
                    num_msat: invoice.amount_msat,
                    timestamp: u64::try_from(now()).unwrap(),
                    expiry: 3600,
                    description: invoice.memo.clone(),
                    description_hash: invoice.description_hash.map(hex::encode).unwrap_or_default(),
                });
        let result = ours
            .or_else(|| state.external.get(&bolt11).cloned())
            .ok_or_else(|| anyhow!("invoice not for current active network"));
        Box::pin(async move { result })
    }

    fn send_payment(&self, request: SendRequest) -> BoxFuture<'_, PaymentStatus> {
        let mut state = self.lock();
        state.sends.push(request);
        let result = state.send_result.clone().ok_or_else(|| anyhow!("payment stream broke"));
        Box::pin(async move { result })
    }

    fn track_payment(&self, _payment_hash: String) -> BoxFuture<'_, PaymentStatus> {
        let status = self.lock().track_result.clone();
        Box::pin(async move { Ok(status) })
    }

    fn subscribe_invoices(&self, _settle_index: u64) -> BoxFuture<'_, Box<dyn InvoiceStream>> {
        let receiver = self.lock().stream.take();
        Box::pin(async move {
            let receiver = receiver.ok_or_else(|| anyhow!("mock stream already taken"))?;
            Ok(Box::new(MockStream(receiver)) as Box<dyn InvoiceStream>)
        })
    }

    fn balances(&self) -> BoxFuture<'_, NodeBalances> {
        let channel_local_msat = self.lock().channel_local_msat;
        Box::pin(async move {
            Ok(NodeBalances {
                channel_local_msat,
                onchain_confirmed_sat: 0,
            })
        })
    }
}
