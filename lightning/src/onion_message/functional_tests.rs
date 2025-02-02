// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

//! Onion message testing and test utilities live here.

use crate::blinded_path::BlindedPath;
use crate::sign::{NodeSigner, Recipient};
use crate::ln::features::InitFeatures;
use crate::ln::msgs::{self, DecodeError, OnionMessageHandler};
use super::{CustomOnionMessageContents, CustomOnionMessageHandler, Destination, MessageRouter, OffersMessage, OffersMessageHandler, OnionMessageContents, OnionMessagePath, OnionMessenger, SendError};
use crate::util::ser::{Writeable, Writer};
use crate::util::test_utils;

use bitcoin::network::constants::Network;
use bitcoin::secp256k1::{PublicKey, Secp256k1};

use crate::io;
use crate::io_extras::read_to_end;
use crate::sync::{Arc, Mutex};

use crate::prelude::*;

struct MessengerNode {
	keys_manager: Arc<test_utils::TestKeysInterface>,
	messenger: OnionMessenger<
		Arc<test_utils::TestKeysInterface>,
		Arc<test_utils::TestKeysInterface>,
		Arc<test_utils::TestLogger>,
		Arc<TestMessageRouter>,
		Arc<TestOffersMessageHandler>,
		Arc<TestCustomMessageHandler>
	>,
	custom_message_handler: Arc<TestCustomMessageHandler>,
}

impl MessengerNode {
	fn get_node_pk(&self) -> PublicKey {
		self.keys_manager.get_node_id(Recipient::Node).unwrap()
	}
}

struct TestMessageRouter {}

impl MessageRouter for TestMessageRouter {
	fn find_path(
		&self, _sender: PublicKey, _peers: Vec<PublicKey>, destination: Destination
	) -> Result<OnionMessagePath, ()> {
		Ok(OnionMessagePath {
			intermediate_nodes: vec![],
			destination,
		})
	}
}

struct TestOffersMessageHandler {}

impl OffersMessageHandler for TestOffersMessageHandler {
	fn handle_message(&self, _message: OffersMessage) -> Option<OffersMessage> {
		None
	}
}

#[derive(Clone, Debug, PartialEq)]
enum TestCustomMessage {
	Request,
	Response,
}

const CUSTOM_REQUEST_MESSAGE_TYPE: u64 = 4242;
const CUSTOM_RESPONSE_MESSAGE_TYPE: u64 = 4343;
const CUSTOM_REQUEST_MESSAGE_CONTENTS: [u8; 32] = [42; 32];
const CUSTOM_RESPONSE_MESSAGE_CONTENTS: [u8; 32] = [43; 32];

impl CustomOnionMessageContents for TestCustomMessage {
	fn tlv_type(&self) -> u64 {
		match self {
			TestCustomMessage::Request => CUSTOM_REQUEST_MESSAGE_TYPE,
			TestCustomMessage::Response => CUSTOM_RESPONSE_MESSAGE_TYPE,
		}
	}
}

impl Writeable for TestCustomMessage {
	fn write<W: Writer>(&self, w: &mut W) -> Result<(), io::Error> {
		match self {
			TestCustomMessage::Request => Ok(CUSTOM_REQUEST_MESSAGE_CONTENTS.write(w)?),
			TestCustomMessage::Response => Ok(CUSTOM_RESPONSE_MESSAGE_CONTENTS.write(w)?),
		}
	}
}

struct TestCustomMessageHandler {
	expected_messages: Mutex<VecDeque<TestCustomMessage>>,
}

impl TestCustomMessageHandler {
	fn new() -> Self {
		Self { expected_messages: Mutex::new(VecDeque::new()) }
	}

	fn expect_message(&self, message: TestCustomMessage) {
		self.expected_messages.lock().unwrap().push_back(message);
	}
}

impl Drop for TestCustomMessageHandler {
	fn drop(&mut self) {
		#[cfg(feature = "std")] {
			if std::thread::panicking() {
				return;
			}
		}
		assert!(self.expected_messages.lock().unwrap().is_empty());
	}
}

impl CustomOnionMessageHandler for TestCustomMessageHandler {
	type CustomMessage = TestCustomMessage;
	fn handle_custom_message(&self, msg: Self::CustomMessage) -> Option<Self::CustomMessage> {
		match self.expected_messages.lock().unwrap().pop_front() {
			Some(expected_msg) => assert_eq!(expected_msg, msg),
			None => panic!("Unexpected message: {:?}", msg),
		}

		match msg {
			TestCustomMessage::Request => Some(TestCustomMessage::Response),
			TestCustomMessage::Response => None,
		}
	}
	fn read_custom_message<R: io::Read>(&self, message_type: u64, buffer: &mut R) -> Result<Option<Self::CustomMessage>, DecodeError> where Self: Sized {
		match message_type {
			CUSTOM_REQUEST_MESSAGE_TYPE => {
				let buf = read_to_end(buffer)?;
				assert_eq!(buf, CUSTOM_REQUEST_MESSAGE_CONTENTS);
				Ok(Some(TestCustomMessage::Request))
			},
			CUSTOM_RESPONSE_MESSAGE_TYPE => {
				let buf = read_to_end(buffer)?;
				assert_eq!(buf, CUSTOM_RESPONSE_MESSAGE_CONTENTS);
				Ok(Some(TestCustomMessage::Response))
			},
			_ => Ok(None),
		}
	}
}

fn create_nodes(num_messengers: u8) -> Vec<MessengerNode> {
	let mut nodes = Vec::new();
	for i in 0..num_messengers {
		let logger = Arc::new(test_utils::TestLogger::with_id(format!("node {}", i)));
		let seed = [i as u8; 32];
		let keys_manager = Arc::new(test_utils::TestKeysInterface::new(&seed, Network::Testnet));
		let message_router = Arc::new(TestMessageRouter {});
		let offers_message_handler = Arc::new(TestOffersMessageHandler {});
		let custom_message_handler = Arc::new(TestCustomMessageHandler::new());
		nodes.push(MessengerNode {
			keys_manager: keys_manager.clone(),
			messenger: OnionMessenger::new(
				keys_manager.clone(), keys_manager, logger.clone(), message_router,
				offers_message_handler, custom_message_handler.clone()
			),
			custom_message_handler,
		});
	}
	for idx in 0..num_messengers - 1 {
		let i = idx as usize;
		let mut features = InitFeatures::empty();
		features.set_onion_messages_optional();
		let init_msg = msgs::Init { features, networks: None, remote_network_address: None };
		nodes[i].messenger.peer_connected(&nodes[i + 1].get_node_pk(), &init_msg.clone(), true).unwrap();
		nodes[i + 1].messenger.peer_connected(&nodes[i].get_node_pk(), &init_msg.clone(), false).unwrap();
	}
	nodes
}

fn pass_along_path(path: &Vec<MessengerNode>) {
	let mut prev_node = &path[0];
	for node in path.into_iter().skip(1) {
		let events = prev_node.messenger.release_pending_msgs();
		let onion_msg =  {
			let msgs = events.get(&node.get_node_pk()).unwrap();
			assert_eq!(msgs.len(), 1);
			msgs[0].clone()
		};
		node.messenger.handle_onion_message(&prev_node.get_node_pk(), &onion_msg);
		prev_node = node;
	}
}

#[test]
fn one_hop() {
	let nodes = create_nodes(2);
	let test_msg = OnionMessageContents::Custom(TestCustomMessage::Response);

	let path = OnionMessagePath {
		intermediate_nodes: vec![],
		destination: Destination::Node(nodes[1].get_node_pk()),
	};
	nodes[0].messenger.send_onion_message(path, test_msg, None).unwrap();
	nodes[1].custom_message_handler.expect_message(TestCustomMessage::Response);
	pass_along_path(&nodes);
}

#[test]
fn two_unblinded_hops() {
	let nodes = create_nodes(3);
	let test_msg = OnionMessageContents::Custom(TestCustomMessage::Response);

	let path = OnionMessagePath {
		intermediate_nodes: vec![nodes[1].get_node_pk()],
		destination: Destination::Node(nodes[2].get_node_pk()),
	};
	nodes[0].messenger.send_onion_message(path, test_msg, None).unwrap();
	nodes[2].custom_message_handler.expect_message(TestCustomMessage::Response);
	pass_along_path(&nodes);
}

#[test]
fn two_unblinded_two_blinded() {
	let nodes = create_nodes(5);
	let test_msg = OnionMessageContents::Custom(TestCustomMessage::Response);

	let secp_ctx = Secp256k1::new();
	let blinded_path = BlindedPath::new_for_message(&[nodes[3].get_node_pk(), nodes[4].get_node_pk()], &*nodes[4].keys_manager, &secp_ctx).unwrap();
	let path = OnionMessagePath {
		intermediate_nodes: vec![nodes[1].get_node_pk(), nodes[2].get_node_pk()],
		destination: Destination::BlindedPath(blinded_path),
	};

	nodes[0].messenger.send_onion_message(path, test_msg, None).unwrap();
	nodes[4].custom_message_handler.expect_message(TestCustomMessage::Response);
	pass_along_path(&nodes);
}

#[test]
fn three_blinded_hops() {
	let nodes = create_nodes(4);
	let test_msg = OnionMessageContents::Custom(TestCustomMessage::Response);

	let secp_ctx = Secp256k1::new();
	let blinded_path = BlindedPath::new_for_message(&[nodes[1].get_node_pk(), nodes[2].get_node_pk(), nodes[3].get_node_pk()], &*nodes[3].keys_manager, &secp_ctx).unwrap();
	let path = OnionMessagePath {
		intermediate_nodes: vec![],
		destination: Destination::BlindedPath(blinded_path),
	};

	nodes[0].messenger.send_onion_message(path, test_msg, None).unwrap();
	nodes[3].custom_message_handler.expect_message(TestCustomMessage::Response);
	pass_along_path(&nodes);
}

#[test]
fn too_big_packet_error() {
	// Make sure we error as expected if a packet is too big to send.
	let nodes = create_nodes(2);
	let test_msg = OnionMessageContents::Custom(TestCustomMessage::Response);

	let hop_node_id = nodes[1].get_node_pk();
	let hops = vec![hop_node_id; 400];
	let path = OnionMessagePath {
		intermediate_nodes: hops,
		destination: Destination::Node(hop_node_id),
	};
	let err = nodes[0].messenger.send_onion_message(path, test_msg, None).unwrap_err();
	assert_eq!(err, SendError::TooBigPacket);
}

#[test]
fn we_are_intro_node() {
	// If we are sending straight to a blinded path and we are the introduction node, we need to
	// advance the blinded path by 1 hop so the second hop is the new introduction node.
	let mut nodes = create_nodes(3);
	let test_msg = TestCustomMessage::Response;

	let secp_ctx = Secp256k1::new();
	let blinded_path = BlindedPath::new_for_message(&[nodes[0].get_node_pk(), nodes[1].get_node_pk(), nodes[2].get_node_pk()], &*nodes[2].keys_manager, &secp_ctx).unwrap();
	let path = OnionMessagePath {
		intermediate_nodes: vec![],
		destination: Destination::BlindedPath(blinded_path),
	};

	nodes[0].messenger.send_onion_message(path, OnionMessageContents::Custom(test_msg.clone()), None).unwrap();
	nodes[2].custom_message_handler.expect_message(TestCustomMessage::Response);
	pass_along_path(&nodes);

	// Try with a two-hop blinded path where we are the introduction node.
	let blinded_path = BlindedPath::new_for_message(&[nodes[0].get_node_pk(), nodes[1].get_node_pk()], &*nodes[1].keys_manager, &secp_ctx).unwrap();
	let path = OnionMessagePath {
		intermediate_nodes: vec![],
		destination: Destination::BlindedPath(blinded_path),
	};
	nodes[0].messenger.send_onion_message(path, OnionMessageContents::Custom(test_msg), None).unwrap();
	nodes[1].custom_message_handler.expect_message(TestCustomMessage::Response);
	nodes.remove(2);
	pass_along_path(&nodes);
}

#[test]
fn invalid_blinded_path_error() {
	// Make sure we error as expected if a provided blinded path has 0 or 1 hops.
	let nodes = create_nodes(3);
	let test_msg = TestCustomMessage::Response;

	// 0 hops
	let secp_ctx = Secp256k1::new();
	let mut blinded_path = BlindedPath::new_for_message(&[nodes[1].get_node_pk(), nodes[2].get_node_pk()], &*nodes[2].keys_manager, &secp_ctx).unwrap();
	blinded_path.blinded_hops.clear();
	let path = OnionMessagePath {
		intermediate_nodes: vec![],
		destination: Destination::BlindedPath(blinded_path),
	};
	let err = nodes[0].messenger.send_onion_message(path, OnionMessageContents::Custom(test_msg.clone()), None).unwrap_err();
	assert_eq!(err, SendError::TooFewBlindedHops);

	// 1 hop
	let mut blinded_path = BlindedPath::new_for_message(&[nodes[1].get_node_pk(), nodes[2].get_node_pk()], &*nodes[2].keys_manager, &secp_ctx).unwrap();
	blinded_path.blinded_hops.remove(0);
	assert_eq!(blinded_path.blinded_hops.len(), 1);
	let path = OnionMessagePath {
		intermediate_nodes: vec![],
		destination: Destination::BlindedPath(blinded_path),
	};
	let err = nodes[0].messenger.send_onion_message(path, OnionMessageContents::Custom(test_msg), None).unwrap_err();
	assert_eq!(err, SendError::TooFewBlindedHops);
}

#[test]
fn reply_path() {
	let mut nodes = create_nodes(4);
	let test_msg = TestCustomMessage::Request;
	let secp_ctx = Secp256k1::new();

	// Destination::Node
	let path = OnionMessagePath {
		intermediate_nodes: vec![nodes[1].get_node_pk(), nodes[2].get_node_pk()],
		destination: Destination::Node(nodes[3].get_node_pk()),
	};
	let reply_path = BlindedPath::new_for_message(&[nodes[2].get_node_pk(), nodes[1].get_node_pk(), nodes[0].get_node_pk()], &*nodes[0].keys_manager, &secp_ctx).unwrap();
	nodes[0].messenger.send_onion_message(path, OnionMessageContents::Custom(test_msg.clone()), Some(reply_path)).unwrap();
	nodes[3].custom_message_handler.expect_message(TestCustomMessage::Request);
	pass_along_path(&nodes);
	// Make sure the last node successfully decoded the reply path.
	nodes[0].custom_message_handler.expect_message(TestCustomMessage::Response);
	nodes.reverse();
	pass_along_path(&nodes);

	// Destination::BlindedPath
	let blinded_path = BlindedPath::new_for_message(&[nodes[1].get_node_pk(), nodes[2].get_node_pk(), nodes[3].get_node_pk()], &*nodes[3].keys_manager, &secp_ctx).unwrap();
	let path = OnionMessagePath {
		intermediate_nodes: vec![],
		destination: Destination::BlindedPath(blinded_path),
	};
	let reply_path = BlindedPath::new_for_message(&[nodes[2].get_node_pk(), nodes[1].get_node_pk(), nodes[0].get_node_pk()], &*nodes[0].keys_manager, &secp_ctx).unwrap();

	nodes[0].messenger.send_onion_message(path, OnionMessageContents::Custom(test_msg), Some(reply_path)).unwrap();
	nodes[3].custom_message_handler.expect_message(TestCustomMessage::Request);
	pass_along_path(&nodes);

	// Make sure the last node successfully decoded the reply path.
	nodes[0].custom_message_handler.expect_message(TestCustomMessage::Response);
	nodes.reverse();
	pass_along_path(&nodes);
}

#[test]
fn invalid_custom_message_type() {
	let nodes = create_nodes(2);

	struct InvalidCustomMessage{}
	impl CustomOnionMessageContents for InvalidCustomMessage {
		fn tlv_type(&self) -> u64 {
			// Onion message contents must have a TLV >= 64.
			63
		}
	}

	impl Writeable for InvalidCustomMessage {
		fn write<W: Writer>(&self, _w: &mut W) -> Result<(), io::Error> { unreachable!() }
	}

	let test_msg = OnionMessageContents::Custom(InvalidCustomMessage {});
	let path = OnionMessagePath {
		intermediate_nodes: vec![],
		destination: Destination::Node(nodes[1].get_node_pk()),
	};
	let err = nodes[0].messenger.send_onion_message(path, test_msg, None).unwrap_err();
	assert_eq!(err, SendError::InvalidMessage);
}

#[test]
fn peer_buffer_full() {
	let nodes = create_nodes(2);
	let test_msg = TestCustomMessage::Request;
	let path = OnionMessagePath {
		intermediate_nodes: vec![],
		destination: Destination::Node(nodes[1].get_node_pk()),
	};
	for _ in 0..188 { // Based on MAX_PER_PEER_BUFFER_SIZE in OnionMessenger
		nodes[0].messenger.send_onion_message(path.clone(), OnionMessageContents::Custom(test_msg.clone()), None).unwrap();
	}
	let err = nodes[0].messenger.send_onion_message(path, OnionMessageContents::Custom(test_msg), None).unwrap_err();
	assert_eq!(err, SendError::BufferFull);
}

#[test]
fn many_hops() {
	// Check we can send over a route with many hops. This will exercise our logic for onion messages
	// of size [`crate::onion_message::packet::BIG_PACKET_HOP_DATA_LEN`].
	let num_nodes: usize = 25;
	let nodes = create_nodes(num_nodes as u8);
	let test_msg = TestCustomMessage::Response;

	let mut intermediate_nodes = vec![];
	for i in 1..(num_nodes-1) {
		intermediate_nodes.push(nodes[i].get_node_pk());
	}

	let path = OnionMessagePath {
		intermediate_nodes,
		destination: Destination::Node(nodes[num_nodes-1].get_node_pk()),
	};
	nodes[0].messenger.send_onion_message(path, OnionMessageContents::Custom(test_msg), None).unwrap();
	nodes[num_nodes-1].custom_message_handler.expect_message(TestCustomMessage::Response);
	pass_along_path(&nodes);
}
