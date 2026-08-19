//! Exact uncovered-demand ownership and active-demand indexes.

use std::collections::BTreeMap;

use crate::plan::DemandKey;
use crate::{PublicKey, Router, Shortfall};

use super::strongest_shortfall;

impl Router {
    pub(crate) fn install_uncovered_ownership(
        &mut self,
        uncovered_by_demand: BTreeMap<DemandKey, BTreeMap<PublicKey, Shortfall>>,
    ) {
        self.uncovered_by_demand = uncovered_by_demand;
        self.uncovered_owners_by_author.clear();
        for (demand, facts) in &self.uncovered_by_demand {
            for (author, fact) in facts {
                self.uncovered_owners_by_author
                    .entry(*author)
                    .or_default()
                    .insert(demand.clone(), *fact);
            }
        }
        self.refresh_uncovered_diagnostics();
    }

    pub(crate) fn replace_uncovered_demand(
        &mut self,
        demand: DemandKey,
        facts: BTreeMap<PublicKey, Shortfall>,
    ) -> bool {
        let unchanged = self
            .uncovered_by_demand
            .get(&demand)
            .is_some_and(|current| current == &facts);
        if unchanged {
            return false;
        }
        let mut changed = self.remove_uncovered_demand(demand.clone());
        if facts.is_empty() {
            return changed;
        }
        for (author, fact) in &facts {
            self.uncovered_owners_by_author
                .entry(*author)
                .or_default()
                .insert(demand.clone(), *fact);
            changed |= self.refresh_uncovered_author(*author);
        }
        self.uncovered_by_demand.insert(demand.clone(), facts);
        changed
    }

    pub(crate) fn remove_uncovered_demand(&mut self, demand: DemandKey) -> bool {
        let Some(facts) = self.uncovered_by_demand.remove(&demand) else {
            return false;
        };
        let mut changed = false;
        for author in facts.keys() {
            if let Some(owners) = self.uncovered_owners_by_author.get_mut(author) {
                owners.remove(&demand);
                if owners.is_empty() {
                    self.uncovered_owners_by_author.remove(author);
                }
            }
            changed |= self.refresh_uncovered_author(*author);
        }
        changed
    }

    fn refresh_uncovered_diagnostics(&mut self) {
        self.last_diag.uncovered_authors = self
            .uncovered_owners_by_author
            .iter()
            .filter_map(|(author, owners)| {
                strongest_shortfall(owners.values()).map(|fact| (*author, fact))
            })
            .collect();
    }

    fn refresh_uncovered_author(&mut self, author: PublicKey) -> bool {
        let next = self
            .uncovered_owners_by_author
            .get(&author)
            .and_then(|owners| strongest_shortfall(owners.values()));
        let previous = self.last_diag.uncovered_authors.get(&author).cloned();
        match next {
            Some(fact) => {
                self.last_diag.uncovered_authors.insert(author, fact);
            }
            None => {
                self.last_diag.uncovered_authors.remove(&author);
            }
        }
        previous != next
    }

}
