//! Array-based hash table matching upstream Neovim `hashtab.c`.

/// Initial capacity for a new hash table.
pub const HT_INIT_SIZE: usize = 16;

/// Magic shift value for Knuth perturbation probing.
pub const PERTURB_SHIFT: usize = 5;

/// Compute the hash value for a byte slice key.
#[must_use]
pub fn hash_key(key: &[u8]) -> usize {
    if key.is_empty() {
        return 0;
    }
    let mut hash = usize::from(key[0]);
    for &b in &key[1..] {
        hash = hash.wrapping_mul(101).wrapping_add(usize::from(b));
    }
    hash
}

/// Trait for items stored in [`HashTable`] that provide a byte key.
pub trait HashItem {
    /// Return the lookup key for this item.
    fn key(&self) -> &[u8];
}

/// A single slot in the hash table array.
#[derive(Clone, Debug, PartialEq)]
pub enum Slot<T> {
    /// Slot has never been used.
    Vacant,
    /// Slot held an item that was removed (tombstone).
    Removed,
    /// Slot holds an active item with its precomputed hash.
    Occupied {
        /// Cached hash of the item's key.
        hash: usize,
        /// Stored item.
        item: T,
    },
}

impl<T> Slot<T> {
    /// Returns true if this slot is empty (either [`Slot::Vacant`] or [`Slot::Removed`]).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        matches!(self, Self::Vacant | Self::Removed)
    }

    /// Returns a reference to the stored item if occupied.
    #[must_use]
    pub const fn item(&self) -> Option<&T> {
        match self {
            Self::Occupied { item, .. } => Some(item),
            Self::Vacant | Self::Removed => None,
        }
    }

    /// Returns a mutable reference to the stored item if occupied.
    #[must_use]
    pub fn item_mut(&mut self) -> Option<&mut T> {
        match self {
            Self::Occupied { item, .. } => Some(item),
            Self::Vacant | Self::Removed => None,
        }
    }
}

enum LookupResult {
    Found(usize),
    NotFound { insert_slot: usize, hash: usize },
}

/// Upstream Neovim-compatible array-based hash table.
#[derive(Clone, Debug)]
pub struct HashTable<T> {
    slots: Vec<Slot<T>>,
    mask: usize,
    used: usize,
    filled: usize,
    locked: usize,
}

impl<T> Default for HashTable<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> HashTable<T> {
    /// Construct an empty table with initial capacity [`HT_INIT_SIZE`].
    #[must_use]
    pub fn new() -> Self {
        let mut slots = Vec::with_capacity(HT_INIT_SIZE);
        for _ in 0..HT_INIT_SIZE {
            slots.push(Slot::Vacant);
        }
        Self {
            slots,
            mask: HT_INIT_SIZE - 1,
            used: 0,
            filled: 0,
            locked: 0,
        }
    }

    /// Number of live items stored in the table.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.used
    }

    /// Whether the table has no live items.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.used == 0
    }

    /// Current slot array capacity.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// Return an iterator over live items in ascending slot order.
    pub fn iter(&self) -> Iter<'_, T> {
        Iter {
            iter: self.slots.iter(),
        }
    }

    /// Return a mutable iterator over live items in ascending slot order.
    pub fn iter_mut(&mut self) -> IterMut<'_, T> {
        IterMut {
            iter: self.slots.iter_mut(),
        }
    }

    /// Reset table to empty initial state.
    pub fn clear(&mut self) {
        self.slots.clear();
        for _ in 0..HT_INIT_SIZE {
            self.slots.push(Slot::Vacant);
        }
        self.mask = HT_INIT_SIZE - 1;
        self.used = 0;
        self.filled = 0;
        self.locked = 0;
    }
}

impl<T: HashItem> HashTable<T> {
    /// Inhibit resize during iteration or bulk operations.
    pub fn lock(&mut self) {
        self.locked = self.locked.saturating_add(1);
    }

    /// Release resize inhibition; triggers pending resize if thresholds are exceeded.
    pub fn unlock(&mut self) {
        self.locked = self.locked.saturating_sub(1);
        self.may_resize();
    }

    /// Return whether resize is currently inhibited.
    #[must_use]
    pub const fn is_locked(&self) -> bool {
        self.locked > 0
    }

    fn lookup(&self, key: &[u8]) -> LookupResult {
        let hash = hash_key(key);
        let mut idx = hash & self.mask;

        match &self.slots[idx] {
            Slot::Vacant => {
                return LookupResult::NotFound {
                    insert_slot: idx,
                    hash,
                };
            }
            Slot::Occupied { hash: h, item } if *h == hash && item.key() == key => {
                return LookupResult::Found(idx);
            }
            _ => {}
        }

        let mut free_slot = matches!(self.slots[idx], Slot::Removed).then_some(idx);

        let mut perturb = hash;
        loop {
            idx = (idx.wrapping_mul(5).wrapping_add(perturb).wrapping_add(1)) & self.mask;
            match &self.slots[idx] {
                Slot::Vacant => {
                    return LookupResult::NotFound {
                        insert_slot: free_slot.unwrap_or(idx),
                        hash,
                    };
                }
                Slot::Occupied { hash: h, item } if *h == hash && item.key() == key => {
                    return LookupResult::Found(idx);
                }
                Slot::Removed => {
                    if free_slot.is_none() {
                        free_slot = Some(idx);
                    }
                }
                _ => {}
            }
            perturb >>= PERTURB_SHIFT;
        }
    }

    /// Look up a live item by key.
    #[must_use]
    pub fn get(&self, key: &[u8]) -> Option<&T> {
        match self.lookup(key) {
            LookupResult::Found(idx) => self.slots[idx].item(),
            LookupResult::NotFound { .. } => None,
        }
    }

    /// Look up a live item mutably by key.
    #[must_use]
    pub fn get_mut(&mut self, key: &[u8]) -> Option<&mut T> {
        match self.lookup(key) {
            LookupResult::Found(idx) => self.slots[idx].item_mut(),
            LookupResult::NotFound { .. } => None,
        }
    }

    /// Insert an item if the key is not already present.
    pub fn insert(&mut self, item: T) -> bool {
        let key = item.key();
        match self.lookup(key) {
            LookupResult::Found(_) => false,
            LookupResult::NotFound { insert_slot, hash } => {
                if matches!(self.slots[insert_slot], Slot::Vacant) {
                    self.filled += 1;
                }
                self.used += 1;
                self.slots[insert_slot] = Slot::Occupied { hash, item };
                self.may_resize();
                true
            }
        }
    }

    /// Set an item, replacing any existing item with the same key.
    pub fn set(&mut self, item: T) {
        let key = item.key();
        match self.lookup(key) {
            LookupResult::Found(idx) => {
                if let Slot::Occupied { item: slot_item, .. } = &mut self.slots[idx] {
                    *slot_item = item;
                }
            }
            LookupResult::NotFound { insert_slot, hash } => {
                if matches!(self.slots[insert_slot], Slot::Vacant) {
                    self.filled += 1;
                }
                self.used += 1;
                self.slots[insert_slot] = Slot::Occupied { hash, item };
                self.may_resize();
            }
        }
    }

    /// Remove an item by key.
    pub fn remove(&mut self, key: &[u8]) -> Option<T> {
        match self.lookup(key) {
            LookupResult::Found(idx) => {
                self.used -= 1;
                let old_slot = std::mem::replace(&mut self.slots[idx], Slot::Removed);
                self.may_resize();
                match old_slot {
                    Slot::Occupied { item, .. } => Some(item),
                    _ => None,
                }
            }
            LookupResult::NotFound { .. } => None,
        }
    }

    fn may_resize(&mut self) {
        if self.locked > 0 {
            return;
        }

        let oldsize = self.slots.len();

        if oldsize == HT_INIT_SIZE && self.filled < HT_INIT_SIZE - 1 {
            return;
        }

        if (self.filled * 3 < oldsize * 2) && (self.used > oldsize / 5) {
            return;
        }

        let minsize = if self.used > 1000 {
            self.used * 2
        } else {
            self.used * 4
        };

        let mut newsize = HT_INIT_SIZE;
        while newsize < minsize {
            newsize <<= 1;
        }

        let newarray_is_small = newsize == HT_INIT_SIZE;

        if !newarray_is_small && newsize == oldsize && (self.filled * 3 < oldsize * 2) {
            return;
        }

        let newmask = newsize - 1;
        let mut new_slots = Vec::with_capacity(newsize);
        for _ in 0..newsize {
            new_slots.push(Slot::Vacant);
        }

        let old_slots = std::mem::replace(&mut self.slots, new_slots);
        for slot in old_slots {
            if let Slot::Occupied { hash, item } = slot {
                let mut newi = hash & newmask;
                if matches!(self.slots[newi], Slot::Occupied { .. }) {
                    let mut perturb = hash;
                    loop {
                        newi = (newi.wrapping_mul(5).wrapping_add(perturb).wrapping_add(1)) & newmask;
                        if matches!(self.slots[newi], Slot::Vacant) {
                            break;
                        }
                        perturb >>= PERTURB_SHIFT;
                    }
                }
                self.slots[newi] = Slot::Occupied { hash, item };
            }
        }

        self.mask = newmask;
        self.filled = self.used;
    }
}

/// Iterator over live items in ascending slot order.
pub struct Iter<'a, T> {
    iter: std::slice::Iter<'a, Slot<T>>,
}

impl<'a, T> Iterator for Iter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        for slot in self.iter.by_ref() {
            if let Slot::Occupied { item, .. } = slot {
                return Some(item);
            }
        }
        None
    }
}

/// Mutable iterator over live items in ascending slot order.
pub struct IterMut<'a, T> {
    iter: std::slice::IterMut<'a, Slot<T>>,
}

impl<'a, T> Iterator for IterMut<'a, T> {
    type Item = &'a mut T;

    fn next(&mut self) -> Option<Self::Item> {
        for slot in self.iter.by_ref() {
            if let Slot::Occupied { item, .. } = slot {
                return Some(item);
            }
        }
        None
    }
}

/// Consuming iterator over live items in ascending slot order.
pub struct IntoIter<T> {
    iter: std::vec::IntoIter<Slot<T>>,
}

impl<T> Iterator for IntoIter<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        for slot in self.iter.by_ref() {
            if let Slot::Occupied { item, .. } = slot {
                return Some(item);
            }
        }
        None
    }
}

impl<T> IntoIterator for HashTable<T> {
    type Item = T;
    type IntoIter = IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        IntoIter {
            iter: self.slots.into_iter(),
        }
    }
}

impl<'a, T> IntoIterator for &'a HashTable<T> {
    type Item = &'a T;
    type IntoIter = Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a, T> IntoIterator for &'a mut HashTable<T> {
    type Item = &'a mut T;
    type IntoIter = IterMut<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

impl<T: HashItem + PartialEq> PartialEq for HashTable<T> {
    fn eq(&self, other: &Self) -> bool {
        if self.used != other.used {
            return false;
        }
        self.iter().all(|item| other.get(item.key()) == Some(item))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq)]
    struct TestItem {
        k: Vec<u8>,
        v: i64,
    }

    impl TestItem {
        fn new(k: &[u8], v: i64) -> Self {
            Self {
                k: k.to_vec(),
                v,
            }
        }
    }

    impl HashItem for TestItem {
        fn key(&self) -> &[u8] {
            &self.k
        }
    }

    #[test]
    fn probe_oracle_three_keys() {
        let mut table = HashTable::new();
        table.insert(TestItem::new(b"z", 1));
        table.insert(TestItem::new(b"a", 2));
        table.insert(TestItem::new(b"m", 3));

        let keys: Vec<&[u8]> = table.iter().map(|item| item.key()).collect();
        assert_eq!(keys, vec![b"a".as_slice(), b"z".as_slice(), b"m".as_slice()]);
    }

    #[test]
    fn probe_oracle_twenty_keys() {
        let mut table = HashTable::new();
        for i in 0..20 {
            let key = format!("k{i}");
            table.insert(TestItem::new(key.as_bytes(), i));
        }

        let keys: Vec<String> = table
            .iter()
            .map(|item| String::from_utf8(item.key().to_vec()).unwrap())
            .collect();
        let expected = vec![
            "k18", "k19", "k0", "k1", "k2", "k3", "k4", "k5", "k6", "k7", "k8", "k9", "k10",
            "k11", "k12", "k13", "k14", "k15", "k16", "k17",
        ];
        assert_eq!(keys, expected);
    }

    #[test]
    fn probe_oracle_dict_with_removals() {
        let mut table = HashTable::new();
        table.insert(TestItem::new(b"a", 1));
        table.insert(TestItem::new(b"b", 2));
        table.insert(TestItem::new(b"c", 3));
        table.insert(TestItem::new(b"d", 4));
        table.insert(TestItem::new(b"e", 5));

        table.remove(b"b");
        table.remove(b"d");
        table.insert(TestItem::new(b"f", 6));

        let keys: Vec<&[u8]> = table.iter().map(|item| item.key()).collect();
        assert_eq!(
            keys,
            vec![
                b"a".as_slice(),
                b"c".as_slice(),
                b"e".as_slice(),
                b"f".as_slice()
            ]
        );
    }

    #[test]
    fn probe_t24_and_t26_slot_layouts() {
        let mut d_t24 = HashTable::new();
        d_t24.insert(TestItem::new(b"p", 1));
        d_t24.insert(TestItem::new(b"f", 2));
        d_t24.insert(TestItem::new(b"v", 3));
        let t24_keys: Vec<&[u8]> = d_t24.iter().map(|item| item.key()).collect();
        assert_eq!(
            t24_keys,
            vec![b"p".as_slice(), b"v".as_slice(), b"f".as_slice()]
        );

        let mut d_t26 = HashTable::new();
        d_t26.insert(TestItem::new(b"v", 3));
        d_t26.insert(TestItem::new(b"f", 2));
        d_t26.insert(TestItem::new(b"p", 1));
        let t26_keys: Vec<&[u8]> = d_t26.iter().map(|item| item.key()).collect();
        assert_eq!(
            t26_keys,
            vec![b"p".as_slice(), b"f".as_slice(), b"v".as_slice()]
        );
    }
}
