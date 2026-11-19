//! pool.rs — DOP-примитивы: индексы вместо указателей, пулы с поколениями.
//!
//! Правила, которые здесь закреплены:
//!  * В модели нет ни одного `&T`/`Box`/`Rc` на элемент чертежа. Ссылка — `Handle`,
//!    то есть (index, generation), 8 байт, копируемый, без владения.
//!  * Поколение ловит «висячую» ссылку после удаления: старый Handle перестаёт
//!    резолвиться, но не роняет процесс (никакого UB, как с висячим указателем).
//!  * Память не отдаётся ОС при удалении: слот уходит в free-list и переиспользуется,
//!    поэтому правка в главном цикле не вызывает malloc.

/// Индекс элемента: слот + поколение. Никакого владения, только адресация.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Handle {
    pub slot: u32,
    pub gen: u32,
}

impl Handle {
    pub const NONE: Handle = Handle { slot: u32::MAX, gen: 0 };
    #[inline]
    pub fn is_none(self) -> bool {
        self.slot == u32::MAX
    }
}

/// Плотный пул: данные лежат подряд (DOP), свободные слоты — в стеке индексов.
pub struct Pool<T> {
    items: Vec<T>,
    gens: Vec<u32>,
    live: Vec<bool>,
    free: Vec<u32>,
}

impl<T: Copy + Default> Pool<T> {
    pub fn with_capacity(n: usize) -> Self {
        Self {
            items: Vec::with_capacity(n),
            gens: Vec::with_capacity(n),
            live: Vec::with_capacity(n),
            free: Vec::with_capacity(n),
        }
    }

    /// Резерв под известное число элементов: после `reserve` вставки не аллоцируют,
    /// пока не превышен потолок. Это и есть «ноль аллокаций в главном цикле».
    pub fn reserve(&mut self, n: usize) {
        self.items.reserve(n.saturating_sub(self.items.len()));
        self.gens.reserve(n.saturating_sub(self.gens.len()));
        self.live.reserve(n.saturating_sub(self.live.len()));
        self.free.reserve(n);
    }

    /// Вставка. `capacity_ok` = false, если пришлось вырасти (сигнал вызывающему).
    pub fn insert(&mut self, v: T) -> (Handle, bool) {
        let before = self.items.capacity();
        if let Some(slot) = self.free.pop() {
            let s = slot as usize;
            self.items[s] = v;
            self.live[s] = true;
            (Handle { slot, gen: self.gens[s] }, self.items.capacity() == before)
        } else {
            let slot = self.items.len() as u32;
            self.items.push(v);
            self.gens.push(1);
            self.live.push(true);
            (Handle { slot, gen: 1 }, self.items.capacity() == before)
        }
    }

    #[inline]
    pub fn get(&self, h: Handle) -> Option<&T> {
        let s = h.slot as usize;
        if s < self.items.len() && self.live[s] && self.gens[s] == h.gen {
            Some(&self.items[s])
        } else {
            None
        }
    }

    #[inline]
    pub fn get_mut(&mut self, h: Handle) -> Option<&mut T> {
        let s = h.slot as usize;
        if s < self.items.len() && self.live[s] && self.gens[s] == h.gen {
            Some(&mut self.items[s])
        } else {
            None
        }
    }

    pub fn remove(&mut self, h: Handle) -> bool {
        let s = h.slot as usize;
        if s < self.items.len() && self.live[s] && self.gens[s] == h.gen {
            self.live[s] = false;
            self.gens[s] = self.gens[s].wrapping_add(1).max(1);
            self.items[s] = T::default();
            self.free.push(h.slot);
            true
        } else {
            false
        }
    }

    /// Плотный обход живых элементов: O(len), без ветвлений на free-list.
    #[inline]
    pub fn iter_live(&self) -> impl Iterator<Item = (Handle, &T)> + '_ {
        self.items
            .iter()
            .enumerate()
            .filter(move |(i, _)| self.live[*i])
            .map(move |(i, v)| (Handle { slot: i as u32, gen: self.gens[i] }, v))
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.live.iter().filter(|l| **l).count()
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.items.capacity()
    }

    pub fn clear_keep_capacity(&mut self) {
        self.items.clear();
        self.gens.clear();
        self.live.clear();
        self.free.clear();
    }
}
