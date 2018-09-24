extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;
extern crate ethabi;
extern crate ethereum_types;
extern crate hex;
extern crate keccak_hash;
extern crate quickersort;
extern crate failure;
#[macro_use]
extern crate failure_derive;

mod eip712;
mod error;

pub use error::*;
pub use eip712::*;

use ethabi::{encode, Token};
use ethereum_types::{Address, U256};
use keccak_hash::{keccak, H256};
use serde_json::Value;
use std::collections::HashSet;
use std::str::FromStr;

/// given a type and HashMap<String, Vec<FieldType>>
/// returns a HashSet of dependent types of the given type
fn build_dependencies<'a>(message_type: &'a str, message_types: &'a MessageTypes) -> Option<(HashSet<&'a str>)>
{
	// get the associated FieldType of the given type
	if let Some(fields) = message_types.get(message_type) {
		let mut deps = HashSet::new();
		deps.insert(message_type);

		for field in fields {
			// seen this type before? skip
			if deps.contains(&*field.type_) {
				continue;
			}
			if let Some(set) = build_dependencies(&field.type_, &message_types) {
				deps.extend(set);
			}
		}
		return Some(deps);
	}
	// primitive types like uint256, address wouldn't exist in MessageTypes
	return None;
}

fn encode_type(message_type: &str, message_types: &MessageTypes) -> Result<String> {
	let deps = {
		if let Some(mut temp) = build_dependencies(message_type, message_types) {
			temp.remove(message_type);
			let mut temp = temp.into_iter().collect::<Vec<_>>();
			quickersort::sort(&mut temp[..]);
			temp.insert(0, message_type);
			temp
		} else {
			return Err(ErrorKind::NonExistentType)?
		}
	};

	let encoded = deps.into_iter().fold(String::new(), |mut acc, dep| {
		// this unwrap is safe because we're asking for a dependency that was recently pulled out of message_types
		let types = message_types.get(dep)
			.unwrap()
			.iter()
			.map(|value| format!("{} {}", value.type_, value.name))
			.collect::<Vec<_>>()
			.join(",");

		acc.push_str(&format!("{}({})", dep, types));
		return acc;
	});

	Ok(encoded)
}

fn type_hash(message_type: &str, typed_data: &MessageTypes) -> Result<H256> {
	Ok(keccak(encode_type(message_type, typed_data)?))
}

fn encode_data(message_type: &str, message_types: &MessageTypes, message: &Value) -> Result<Vec<u8>> {
	let type_hash = (&type_hash(message_type, &message_types)?).to_vec();
	let mut tokens = vec![Token::FixedBytes(type_hash)];
	for field in message_types.get(message_type).ok_or_else(|| ErrorKind::NonExistentType)? {
		let value = &message[&field.name];
		match &*field.type_ {
			// Array type e.g uint256[], string[]
			ty if ty.rfind(']') == Some(ty.len() - 1) => {
				let array_type = ty.split('[').collect::<Vec<_>>()[0];
				let mut items = vec![];
				for item in value.as_array().ok_or_else(|| serde_error("array", &field.name))? {
					let encoded = encode_data(array_type.into(), &message_types, item)?;
					items.push(encoded);
				}
				tokens.push(Token::FixedBytes(keccak(items.concat()).0.to_vec()));
			}
			"string" | "bytes32" => {
				let value = value.as_str().ok_or_else(|| serde_error("string", &field.name))?;
				let hash = (&keccak(value)).to_vec();
				tokens.push(Token::FixedBytes(hash));
			}
			"bool" => tokens.push(Token::Bool(value.as_bool().ok_or_else(|| serde_error("bool", &field.name))?)),
			"address" => {
				let addr = value.as_str().ok_or_else(|| serde_error("string", &field.name))?;
				if addr.len() != 42 {
					return Err(ErrorKind::InvalidAddressLength(addr.len()))?
				}
				// we've checked the address length, this is safe
				let  addr = addr.get(2..).unwrap();
				let address = Address::from_str(addr).map_err(|err| ErrorKind::HexParseError(format!("{}", err)))?;
				tokens.push(Token::Address(address));
			}
			// custom type defined in message types
			t if message_types.get(t).is_some() => {
				let encoded = encode_data(&field.type_, &message_types, &value)?;
				let hash = (&keccak(encoded)).to_vec();
				tokens.push(Token::FixedBytes(hash));
			}
			// (un)signed integers
			ty if ty.starts_with("uint") || ty.starts_with("int") => {
				print!("{}", value);
				// try to deserialize as a number first, then a string
				let uint = match (value.as_u64(), value.as_str()){
					(Some(number), _) => U256::from(number),
					(_, Some(string)) => {
						U256::from_str(string).map_err(|err| ErrorKind::HexParseError(format!("{}", err)))?
					},
					_ => return Err(serde_error("int/uint", &field.name))?
				};
				tokens.push(Token::Uint(uint));
			}
			// the type couldn't be encoded
			_ => return Err(ErrorKind::UnknownType(field.name.to_owned(), field.name.to_owned()))?
		}
	}
	return Ok(encode(&tokens));
}

pub fn hash_data(typed_data: EIP712) -> Result<Vec<u8>> {
	// json schema validation logic!
	// EIP-191 compliant
	let prefix = (b"\x19\x01").to_vec();
	let (domain_hash, data_hash) = (
		keccak(encode_data("EIP712Domain", &typed_data.types, &typed_data.domain)?).0,
		keccak(encode_data(&typed_data.primary_type, &typed_data.types, &typed_data.message)?).0
	);
	let concat = [&prefix[..], &domain_hash[..], &data_hash[..]].concat();
	Ok((&keccak(concat)).to_vec())
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::from_str;

	const JSON: &'static str = r#"{
		"primaryType": "Mail",
		"domain": {
			"name": "Ether Mail",
			"version": "1",
			"chainId": 1,
			"verifyingContract": "0xCcCCccccCCCCcCCCCCCcCcCccCcCCCcCcccccccC"
		},
		"message": {
			"from": {
				"name": "Cow",
				"wallet": "0xCD2a3d9F938E13CD947Ec05AbC7FE734Df8DD826"
			},
			"to": {
				"name": "Bob",
				"wallet": "0xbBbBBBBbbBBBbbbBbbBbbbbBBbBbbbbBbBbbBBbB"
			},
			"contents": "Hello, Bob!"
		},
		"types": {
			"EIP712Domain": [
			    { "name": "name", "type": "string" },
				{ "name": "version", "type": "string" },
				{ "name": "chainId", "type": "uint256" },
				{ "name": "verifyingContract", "type": "address" }
			],
			"Person": [
				{ "name": "name", "type": "string" },
				{ "name": "wallet", "type": "address" }
			],
			"Mail": [
				{ "name": "from", "type": "Person" },
				{ "name": "to", "type": "Person" },
				{ "name": "contents", "type": "string" }
			]
		}
	}"#;

	#[test]
	fn test_build_dependencies() {
		let string = r#"{
			"EIP712Domain": [
				{ "name": "name", "type": "string" },
				{ "name": "version", "type": "string" },
				{ "name": "chainId", "type": "uint256" },
				{ "name": "verifyingContract", "type": "address" }
			],
			"Person": [
				{ "name": "name", "type": "string" },
				{ "name": "wallet", "type": "address" }
			],
			"Mail": [
				{ "name": "from", "type": "Person" },
				{ "name": "to", "type": "Person" },
				{ "name": "contents", "type": "string" }
			]
		}"#;

		let value = from_str::<MessageTypes>(string).expect("alas error!");
		let mail = "Mail";
		let person = "Person";

		let hashset = {
			let mut temp = HashSet::new();
			temp.insert(mail);
			temp.insert(person);
			temp
		};
		assert_eq!(build_dependencies(mail, &value), Some(hashset));
	}

	#[test]
	fn test_encode_type() {
		let string = r#"{
			"EIP712Domain": [
				{ "name": "name", "type": "string" },
				{ "name": "version", "type": "string" },
				{ "name": "chainId", "type": "uint256" },
				{ "name": "verifyingContract", "type": "address" }
			],
			"Person": [
				{ "name": "name", "type": "string" },
				{ "name": "wallet", "type": "address" }
			],
			"Mail": [
				{ "name": "from", "type": "Person" },
				{ "name": "to", "type": "Person" },
				{ "name": "contents", "type": "string" }
			]
		}"#;

		let value = from_str::<MessageTypes>(string).expect("alas error!");
		let mail = &String::from("Mail");
		assert_eq!(
			"Mail(Person from,Person to,string contents)Person(string name,address wallet)",
			encode_type(&mail, &value).expect("alas error!")
		)
	}

	#[test]
	fn test_encode_type_hash() {
		let string = r#"{
			"EIP712Domain": [
				{ "name": "name", "type": "string" },
				{ "name": "version", "type": "string" },
				{ "name": "chainId", "type": "uint256" },
				{ "name": "verifyingContract", "type": "address" }
			],
			"Person": [
				{ "name": "name", "type": "string" },
				{ "name": "wallet", "type": "address" }
			],
			"Mail": [
				{ "name": "from", "type": "Person" },
				{ "name": "to", "type": "Person" },
				{ "name": "contents", "type": "string" }
			]
		}"#;

		let value = from_str::<MessageTypes>(string).expect("alas error!");
		let mail = &String::from("Mail");
		let hash = hex::encode(type_hash(&mail, &value).expect("alas error!").0);
		assert_eq!(
			hash,
			"a0cedeb2dc280ba39b857546d74f5549c3a1d7bdc2dd96bf881f76108e23dac2"
		);
	}

	#[test]
	fn test_encode_data() {
		let typed_data = from_str::<EIP712>(JSON).expect("alas error!");

		let encoded = encode_data("Mail".into(), &typed_data.types, &typed_data.message).expect("alas error!");
		assert_eq!(hex::encode(encoded), "a0cedeb2dc280ba39b857546d74f5549c3a1d7bdc2dd96bf881f76108e23dac2fc71e5fa27ff56c350aa531bc129ebdf613b772b6604664f5d8dbe21b85eb0c8cd54f074a4af31b4411ff6a60c9719dbd559c221c8ac3492d9d872b041d703d1b5aadf3154a261abdd9086fc627b61efca26ae5702701d05cd2305f7c52a2fc8")
	}

	#[test]
	fn test_hash_data() {
		let typed_data = from_str::<EIP712>(JSON).expect("alas error!");
		assert_eq!(
			hex::encode(hash_data(typed_data).expect("alas error!")),
			"be609aee343fb3c4b28e1df9e632fca64fcfaede20f02e86244efddf30957bd2"
		)
	}
}
