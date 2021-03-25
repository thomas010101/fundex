use ethabi::{Contract, Event, Function, ParamType};
use tiny_keccak::Keccak;
use web3::types::H256;

/// Hashes a string to a H256 hash.
pub fn string_to_h256(s: &str) -> H256 {
    let mut result = [0u8; 32];
    let data = s.replace(" ", "").into_bytes();
    let mut sponge = Keccak::new_keccak256();
    sponge.update(&data);
    sponge.finalize(&mut result);

    // This was deprecated but the replacement seems to not be available in the
    // version web3 uses.
    #[allow(deprecated)]
    H256::from_slice(&result)
}

/// Returns a `(uint256,address)` style signature for a tuple type.
fn tuple_signature(components: &Vec<Box<ParamType>>) -> String {
    format!(
        "({})",
        components
            .iter()
            .map(|component| event_param_type_signature(&component))
            .collect::<Vec<_>>()
            .join(",")
    )
}

/// Returns the signature of an event parameter type (e.g. `uint256`).
fn event_param_type_signature(kind: &ParamType) -> String {
    use ParamType::*;

    match kind {
        Address => "address".into(),
        Bytes => "bytes".into(),
        Int(size) => format!("int{}", size),
        Uint(size) => format!("uint{}", size),
        Bool => "bool".into(),
        String => "string".into(),
        Array(inner) => format!("{}[]", event_param_type_signature(&*inner)),
        FixedBytes(size) => format!("bytes{}", size),
        FixedArray(inner, size) => format!("{}[{}]", event_param_type_signature(&*inner), size),
        Tuple(components) => tuple_signature(&components),
    }
}

/// Returns an `Event(uint256,address)` signature for an event, without `indexed` hints.
fn ambiguous_event_signature(event: &Event) -> String {
    format!(
        "{}({})",
        event.name,
        event
            .inputs
            .iter()
            .map(|input| format!("{}", event_param_type_signature(&input.kind)))
            .collect::<Vec<_>>()
            .join(",")
    )
}

/// Returns an `Event(indexed uint256,address)` type signature for an event.
fn event_signature(event: &Event) -> String {
    format!(
        "{}({})",
        event.name,
        event
            .inputs
            .iter()
            .map(|input| format!(
                "{}{}",
                if input.indexed { "indexed " } else { "" },
                event_param_type_signature(&input.kind)
            ))
            .collect::<Vec<_>>()
            .join(",")
    )
}

/// Returns the contract event with the given signature, if it exists.
pub fn contract_event_with_signature<'a>(
    contract: &'a Contract,
    signature: &str,
) -> Option<&'a Event> {
    contract
        .events()
        .find(|event| event_signature(event) == signature)
        .or_else(|| {
            let parens = signature.find("(").unwrap_or(0);
            let event_name = &signature[0..parens];

            let matching_events = contract
                .events()
                .filter(|event| event.name == event_name)
                .collect::<Vec<_>>();

            if matching_events.len() == 1
                && ambiguous_event_signature(matching_events[0]) == signature
            {
                Some(matching_events[0])
            } else {
                None
            }
        })
}

pub fn contract_function_with_signature<'a>(
    contract: &'a Contract,
    target_signature: &str,
) -> Option<&'a Function> {
    contract
        .functions()
        .filter(|function| match function.state_mutability {
            ethabi::StateMutability::Payable | ethabi::StateMutability::NonPayable => true,
            ethabi::StateMutability::Pure | ethabi::StateMutability::View => false,
        })
        .find(|function| {
            // Construct the argument function signature:
            // `address,uint256,bool`
            let mut arguments = function
                .inputs
                .iter()
                .map(|input| format!("{}", input.kind))
                .collect::<Vec<String>>()
                .join(",");
            // `address,uint256,bool)
            arguments.push_str(")");
            // `operation(address,uint256,bool)`
            let actual_signature = vec![function.name.clone(), arguments].join("(");
            target_signature == actual_signature
        })
}
