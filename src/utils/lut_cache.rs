use solana_program::address_lookup_table::AddressLookupTableAccount;
use solana_sdk::{address_lookup_table::state::AddressLookupTable, pubkey::Pubkey};
use std::collections::HashMap;

pub struct LutCache {
    map: HashMap<Pubkey, AddressLookupTableAccount>,
}

impl LutCache {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    fn insert(&mut self, alt: AddressLookupTableAccount) {
        self.map.insert(alt.key, alt);
    }

    pub fn fetch_missing(
        &mut self,
        rpc: &solana_client::rpc_client::RpcClient,
        addrs: &[Pubkey],
    ) -> anyhow::Result<Vec<AddressLookupTableAccount>> {
        let missing: Vec<Pubkey> = addrs
            .iter()
            .copied()
            .filter(|k| !self.map.contains_key(k))
            .collect();
        if missing.is_empty() {
            return Ok(addrs
                .iter()
                .filter_map(|k| self.map.get(k).cloned())
                .collect());
        }

        let accs = rpc.get_multiple_accounts(&missing)?;
        let mut out = Vec::with_capacity(addrs.len());
        for (pk, acc_opt) in missing.into_iter().zip(accs.into_iter()) {
            if let Some(acc) = acc_opt {
                if let Ok(state) = AddressLookupTable::deserialize(&acc.data) {
                    let alt = AddressLookupTableAccount {
                        key: pk,
                        addresses: state.addresses.to_vec(),
                    };
                    self.insert(alt.clone());
                    out.push(alt);
                }
            }
        }
        // return all requested (cached + newly fetched) in original order
        Ok(addrs
            .iter()
            .filter_map(|k| self.map.get(k).cloned())
            .collect())
    }
}
