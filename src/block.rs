use crate::constants::*;
use crate::object::*;
use crate::util::*;
/// LineMap is used for scanning block for holes
///
/// 根据论文这里实际上应该是一个byte代表一个line，因为使用bit会导致并发安全问题（data race）
///
/// TODO 这里原作者使用了bitmap，实际上使用byte就可以直接避免datarace[[1]][[2]]，这里应该使用bytemap
///
/// [1]: https://stackoverflow.com/questions/19903338/c-memory-model-and-race-conditions-on-char-arrays
/// [2]: https://stackoverflow.com/questions/46916696/on-a-64-bit-machine-can-i-safely-operate-on-individual-bytes-of-a-64-bit-quadwo
pub struct LineMap {
    bitmap_: [usize; Self::BITMAP_SIZE / core::mem::size_of::<usize>()],
}
impl LineMap {
    pub fn clear_all(&mut self) {
        for byte in self.bitmap_.iter_mut() {
            *byte = 0;
        }
    }
    pub fn is_empty(&self) -> bool {
        for byte in self.bitmap_.iter() {
            if *byte != 0 {
                return false;
            }
        }
        true
    }
    #[inline]
    pub fn visit_marked_range(
        &self,
        heap_begin: usize,
        visit_begin: usize,
        visit_end: usize,
        mut visitor: impl FnMut(usize),
    ) {
        let offset_start = visit_begin - heap_begin;
        let offset_end = visit_end - heap_begin;
        let index_start = Self::offset_to_index(offset_start);
        let index_end = Self::offset_to_index(offset_end);
        let bit_start = (offset_start / LINE_SIZE) * (core::mem::size_of::<usize>() * 8);
        let bit_end = (offset_end / LINE_SIZE) * (core::mem::size_of::<usize>() * 8);
        let mut left_edge = self.bitmap_[index_start];
        left_edge &= !((1 << bit_start) - 1);
        let mut right_edge;
        if index_start < index_end {
            if left_edge != 0 {
                let ptr_base = Self::index_to_offset(index_start) as usize + heap_begin;
                while {
                    let shift = left_edge.trailing_zeros() as usize;
                    let obj = ptr_base + shift * LINE_SIZE;
                    visitor(obj);
                    left_edge ^= 1 << shift;
                    left_edge != 0
                } {}
            }
            for i in index_start + 1..index_end {
                let mut w = self.bitmap_[i];
                if w != 0 {
                    let ptr_base = Self::index_to_offset(i) as usize + heap_begin;
                    while {
                        let shift = w.trailing_zeros() as usize;
                        let obj = ptr_base + shift * LINE_SIZE;
                        visitor(obj);
                        w ^= 1 << shift;
                        w != 0
                    } {}
                }
            }
            if bit_end == 0 {
                right_edge = 0;
            } else {
                right_edge = self.bitmap_[index_end];
            }
        } else {
            right_edge = left_edge;
        }
        right_edge &= (1 << bit_end) - 1;
        if right_edge != 0 {
            let ptr_base = Self::index_to_offset(index_end) as usize + heap_begin;
            while {
                let shift = right_edge.trailing_zeros() as usize;
                let obj = ptr_base + shift * LINE_SIZE;
                visitor(obj);
                right_edge ^= 1 << shift;
                right_edge != 0
            } {}
        }
    }
    pub const BITMAP_SIZE: usize = {
        let bytes_covered_per_word = LINE_SIZE * (core::mem::size_of::<usize>() * 8);
        (crate::util::round_up(BLOCK_SIZE as u64, bytes_covered_per_word as _)
            / bytes_covered_per_word as u64) as usize
            * core::mem::size_of::<isize>()
    };
    pub const fn offset_bit_index(offset: usize) -> usize {
        (offset / LINE_SIZE) % (core::mem::size_of::<usize>() * 8)
    }
    pub const fn offset_to_index(offset: usize) -> usize {
        offset / LINE_SIZE / (core::mem::size_of::<usize>() * 8)
    }
    pub const fn index_to_offset(index: usize) -> isize {
        return index as isize * LINE_SIZE as isize * (core::mem::size_of::<usize>() as isize * 8);
    }
    pub const fn offset_to_mask(offset: usize) -> usize {
        1 << ((offset / LINE_SIZE) % (core::mem::size_of::<usize>() * 8))
    }
    #[inline(always)]
    pub fn test(&self, object: usize, heap_begin: usize) -> bool {
        let offset = object - heap_begin;
        let index = Self::offset_to_index(offset as _);
        let mask = Self::offset_to_mask(offset as _);
        let entry = self.bitmap_[index as usize];
        (entry & mask) != 0
    }
    #[inline(always)]
    pub fn set(&mut self, object: usize, heap_begin: usize) -> bool {
        let offset = object - heap_begin;
        let index = Self::offset_to_index(offset as _);
        let mask = Self::offset_to_mask(offset as _);
        let entry = &mut self.bitmap_[index as usize];
        if (*entry & mask) == 0 {
            *entry |= mask;
            return true;
        }
        false
    }
    #[inline(always)]
    pub fn clear(&mut self, object: usize, heap_begin: usize) -> bool {
        let offset = object - heap_begin;
        let index = Self::offset_to_index(offset as _);
        let mask = Self::offset_to_mask(offset as _);
        let entry = &mut self.bitmap_[index as usize];
        if (*entry & mask) != 0 {
            *entry &= !mask;
            return true;
        }
        false
    }
    #[inline(always)]
    pub fn new() -> Self {
        let b = [0usize; Self::BITMAP_SIZE / core::mem::size_of::<usize>()];
        let this = Self { bitmap_: b };
        this
    }
}
/// 其实字段只有Block的metadata，数据区域在最后一个字段地址后
#[repr(C)]
pub struct ImmixBlock {
    /// Bitmap for marking lines
    pub line_map: LineMap,
    /// Bitmap of objects used for conservative marking
    /// pub object_map: ObjectMap,
    /// Is this block actually allocated
    pub allocated: bool,
    /// How many holes in this block
    pub hole_count: u32,
    pub evacuation_candidate: bool,
    //pub map: memmap::MmapMut,
}

impl ImmixBlock {
    /// Get pointer to block from `object` pointer.
    ///
    /// Block地址一定被是BLOCK_SIZE的整数倍，利用此性质进行计算
    ///
    /// # Safety
    /// Does not do anything unsafe but might return wrong pointer
    pub unsafe fn get_block_ptr(object: Address) -> *mut Self {
        let off = object.to_usize() % BLOCK_SIZE;
        (object.to_mut_ptr::<u8>()).offset(-(off as isize)) as *mut ImmixBlock
    }
    /*pub fn set_gc_object(&mut self, addr: Address) -> bool {
        unsafe {
            //let f = addr.to_mut_ptr::<[u64; 2]>().read();
            let x = self.object_map.set(addr.to_usize(), self.begin());
            //debug_assert!(addr.to_mut_ptr::<[u64; 2]>().read() == f);
            x
        }
    }
    pub fn unset_gc_object(&mut self, addr: Address) -> bool {
        self.object_map.clear(addr.to_usize(), self.begin())
    }*/
    /// 输入的at是已经分配好的block实际指针
    pub fn new(at: *mut u8) -> &'static mut Self {
        unsafe {
            let ptr = at as *mut Self;
            debug_assert!(ptr as usize % BLOCK_SIZE == 0);
            ptr.write(Self {
                line_map: LineMap::new(),
                //object_map: ObjectMap::new(),
                allocated: false,
                hole_count: 0,
                evacuation_candidate: false,
            });

            &mut *ptr
        }
    }
    #[inline]
    pub fn is_in_block(&self, p: Address) -> bool {
        if self.allocated {
            let b = self.begin();
            let e = b + BLOCK_SIZE;
            b < p.to_usize() && p.to_usize() <= e
        } else {
            false
        }
    }
    /*#[inline]
    pub fn is_gc_object(&self, p: Address) -> bool {
        if self.is_in_block(p) {
            self.object_map.test(p.to_usize(), self.begin())
        } else {
            false
        }
    }*/
    pub fn begin(&self) -> usize {
        self as *const Self as usize
    }
    /// Scan the block for a hole to allocate into.
    ///
    /// The scan will start at `last_high_offset` bytes into the block and
    /// return a tuple of `low_offset`, `high_offset` as the lowest and
    /// highest usable offsets for a hole.
    ///
    /// `None` is returned if no hole was found.
    ///
    /// ## 保守标记
    /// 因为一个object可能会使用多个line，所以标记的时候需要遍历
    /// object的空间，标记所有的被包含line。但是这么做性能不好，所以引入保守标记  
    ///
    /// 实践证明大部分object小于两个line，所以对于小object，我们只标记第一个line，默认认为第二个line被标记了。
    /// 对于中object我们才对line进行遍历标记
    ///
    /// TODO：原论文的表述是大部分object小于128字节，那么应该认为大部分对象是能放在一个line里的，那默认让他占两个
    /// line是不是很浪费空间？性能优化是不是有限？（待验证）
    pub fn scan_block(&self, last_high_offset: u16) -> Option<(u16, u16)> {
        let last_high_index = last_high_offset as usize / LINE_SIZE;
        let mut low_index = NUM_LINES_PER_BLOCK - 1;
        /*debug!(
            "Scanning block {:p} for a hole with last_high_offset {}",
            self, last_high_index
        );*/
        // 保守标记，起始line需要+1
        for index in (last_high_index + 1)..NUM_LINES_PER_BLOCK {
            if !self
                .line_map
                .test(self.begin() + (index * LINE_SIZE), self.begin())
            {
                low_index = index + 1;
                break;
            }
        }
        let mut high_index = NUM_LINES_PER_BLOCK;
        for index in low_index..NUM_LINES_PER_BLOCK {
            if self
                .line_map
                .test(self.begin() + (LINE_SIZE * index), self.begin())
            {
                high_index = index;
                break;
            }
        }

        if low_index == high_index && high_index != (NUM_LINES_PER_BLOCK - 1) {
            //debug!("Rescan: Found single line hole? in block {:p}", self);
            return self.scan_block((high_index * LINE_SIZE - 1) as u16);
        } else if low_index < (NUM_LINES_PER_BLOCK - 1) {
            /* debug!(
                "Found low index {} and high index {} in block {:p}",
                low_index, high_index, self
            );*/

            /*debug!(
                "Index offsets: ({},{})",
                low_index * LINE_SIZE,
                high_index * LINE_SIZE - 1
            );*/
            return Some((
                align_usize(low_index * LINE_SIZE, 16) as u16,
                (high_index * LINE_SIZE - 1) as u16,
            ));
        }
        //debug!("Found no hole in block {:p}", self);

        None
    }
    pub fn count_holes(&mut self) -> usize {
        let mut holes: usize = 0;
        let mut in_hole = false;
        let b = self.begin();
        for i in 0..NUM_LINES_PER_BLOCK {
            match (in_hole, self.line_map.test(b + (LINE_SIZE * i), b)) {
                (false, false) => {
                    holes += 1;
                    in_hole = true;
                }
                (_, _) => {
                    in_hole = false;
                }
            }
        }
        self.hole_count = holes as _;
        holes
    }
    pub fn offset(&self, offset: usize) -> Address {
        Address::from(self.begin() + offset)
    }

    pub fn is_empty(&self) -> bool {
        for i in 0..NUM_LINES_PER_BLOCK {
            if self
                .line_map
                .test(self.begin() + (i * LINE_SIZE), self.begin())
            {
                return false;
            }
        }
        true
    }
    /// Update the line counter for the given object.
    ///
    /// Increment if `increment`, otherwise do a saturating substraction.
    #[inline(always)]
    fn modify_line(&mut self, object: Address, mark: bool) {
        let line_num = Self::object_to_line_num(object);
        let b = self.begin();

        let object_ptr = object.to_mut_ptr::<RawGc>();
        unsafe {
            let obj = &mut *object_ptr;

            let size = obj.object_size();

            for line in line_num..(line_num + (size / LINE_SIZE) + 1) {
                if mark {
                    self.line_map.set(b + (line * LINE_SIZE), b);
                    //debug_assert!(self.line_map.test(b + (line * LINE_SIZE), b));
                } else {
                    self.line_map.clear(b + (line * LINE_SIZE), b);
                }
            }
        }
    }
    /// Return the number of holes and marked lines in this block.
    ///
    /// A marked line is a line with a count of at least one.
    ///
    /// _Note_: You must call count_holes() bevorhand to set the number of
    /// holes.
    pub fn count_holes_and_marked_lines(&self) -> (usize, usize) {
        (self.hole_count as usize, {
            let mut count = 0;
            for line in 0..NUM_LINES_PER_BLOCK {
                if self
                    .line_map
                    .test(line * LINE_SIZE + self.begin(), self.begin())
                {
                    count += 1;
                }
            }
            count
        })
    }

    /// Return the number of holes and available lines in this block.
    ///
    /// An available line is a line with a count of zero.
    ///
    /// _Note_: You must call count_holes() bevorhand to set the number of
    /// holes.
    pub fn count_holes_and_available_lines(&self) -> (usize, usize) {
        (self.hole_count as usize, {
            let mut count = 0;
            for line in 0..NUM_LINES_PER_BLOCK {
                if !self
                    .line_map
                    .test(line * LINE_SIZE + self.begin(), self.begin())
                {
                    count += 1;
                }
            }
            count
        })
    }
    pub fn reset(&mut self) {
        self.line_map.clear_all();
        // self.object_map.clear_all();
        self.allocated = false;
        self.hole_count = 0;
        self.evacuation_candidate = false;
    }
    pub fn line_object_mark(&mut self, object: Address) {
        self.modify_line(object, true);
    }

    pub fn line_object_unmark(&mut self, object: Address) {
        self.modify_line(object, false);
    }
    pub fn line_is_marked(&self, line: usize) -> bool {
        self.line_map
            .test(self.begin() + (line * LINE_SIZE), self.begin())
    }

    pub fn object_to_line_num(object: Address) -> usize {
        (object.to_usize() % BLOCK_SIZE) / LINE_SIZE
    }
}
