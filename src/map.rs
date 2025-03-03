//! A module for storing key-value pairs in flash with minimal erase cycles.
//!
//! Basic API:
//!
//! ```rust,ignore
//! enum MyCustomType {
//!     X,
//!     Y,
//!     // ...
//! }
//!
//! impl StorageItem for MyCustomType {
//!     // ...
//! }
//!
//! let mut flash = SomeFlashChip::new();
//! let flash_range = 0x1000..0x2000; // These are the flash addresses in which the crate will operate
//!
//! assert_eq!(
//!     fetch_item::<MyCustomType, SomeFlashChip>(
//!         &mut flash,
//!         flash_range.clone(),
//!         0
//!     ).unwrap(),
//!     None
//! );
//!
//! store_item::<MyCustomType, SomeFlashChip>(
//!     &mut flash,
//!     flash_range.clone(),
//!     MyCustomType::X
//! ).unwrap();
//!
//! assert_eq!(
//!     fetch_item::<MyCustomType, SomeFlashChip>(
//!         &mut flash,
//!         flash_range.clone(),
//!         0
//!     ).unwrap(),
//!     Some(MyCustomType::X)
//! );
//! ```

use core::cell::RefCell;

use super::*;

/// Get a storage item from the flash.
/// Only the last stored item of the given key is returned.
///
/// If no value with the key is found, None is returned.
pub fn fetch_item<I: StorageItem, S: NorFlash>(
    flash: &mut S,
    flash_range: Range<u32>,
    search_key: I::Key,
) -> Result<Option<I>, MapError<I::Error, S::Error>> {
    Ok(fetch_item_with_location(flash, flash_range, search_key)?.map(|(item, _, _)| item))
}

/// Fetch the item, but with the address and the length too
#[allow(clippy::type_complexity)]
fn fetch_item_with_location<I: StorageItem, S: NorFlash>(
    flash: &mut S,
    flash_range: Range<u32>,
    search_key: I::Key,
) -> Result<Option<(I, u32, usize)>, MapError<I::Error, S::Error>> {
    assert_eq!(flash_range.start % S::ERASE_SIZE as u32, 0);
    assert_eq!(flash_range.end % S::ERASE_SIZE as u32, 0);
    assert!(flash_range.end - flash_range.start >= S::ERASE_SIZE as u32 * 2);

    assert!(S::ERASE_SIZE >= S::WRITE_SIZE * 3);
    assert_eq!(S::READ_SIZE, 1);

    // We need to find the page we were last using. This should be the only partial open page.
    let mut last_used_page =
        find_first_page(flash, flash_range.clone(), 0, PageState::PartialOpen)?;

    #[cfg(feature = "defmt")]
    defmt::trace!("Fetch item, last used page: {}", last_used_page);

    if last_used_page.is_none() {
        // In the event that all pages are still open or the last used page was just closed, we search for the first open page.
        // If the page one before that is closed, then that's the last used page.
        if let Some(first_open_page) =
            find_first_page(flash, flash_range.clone(), 0, PageState::Open)?
        {
            let previous_page = previous_page::<S>(flash_range.clone(), first_open_page);
            if get_page_state(flash, flash_range.clone(), previous_page)?.is_closed() {
                last_used_page = Some(previous_page);
            } else {
                // The page before the open page is not closed, so it must be open.
                // This means that all pages are open and that we don't have any items yet.
                return Ok(None);
            }
        } else {
            // There are no open pages, so everything must be closed.
            // Something is up and this should never happen.
            // To recover, we will just erase all the flash.
            return Err(MapError::Corrupted);
        }
    }

    // We must now find the most recent storage item with the key that was asked for.
    // If we don't find it in the current page, then we check again in the previous page if that page is closed.

    let mut current_page_to_check = last_used_page.unwrap();
    let mut newest_found_item = None;

    let flash = RefCell::new(flash);

    loop {
        for found_item_result in
            read_page_items::<I, S>(&flash, flash_range.clone(), current_page_to_check)?
        {
            let found_item = found_item_result?;
            if found_item.0.key() == search_key {
                newest_found_item = Some(found_item);
            }
        }

        // We've found the item! We can stop searching
        if newest_found_item.is_some() {
            break;
        }

        // We have not found the item. We've got to look in the previous page, but only if that page is closed and contains data.
        let previous_page = previous_page::<S>(flash_range.clone(), current_page_to_check);

        if get_page_state(*flash.borrow_mut(), flash_range.clone(), previous_page)?
            != PageState::Closed
        {
            // We've looked through all the pages with data and couldn't find the item
            return Ok(None);
        }

        current_page_to_check = previous_page;
    }

    Ok(newest_found_item)
}

/// Store an item into flash memory.
/// It will overwrite the last value that has the same key.
/// The flash needs to be at least 2 pages long.
pub fn store_item<I: StorageItem, S: NorFlash>(
    flash: &mut S,
    flash_range: Range<u32>,
    item: I,
) -> Result<(), MapError<I::Error, S::Error>> {
    assert_eq!(flash_range.start % S::ERASE_SIZE as u32, 0);
    assert_eq!(flash_range.end % S::ERASE_SIZE as u32, 0);

    assert!(flash_range.len() / S::ERASE_SIZE >= 2);

    assert!(S::ERASE_SIZE >= S::WRITE_SIZE * 3);
    assert_eq!(S::READ_SIZE, 1);

    return store_item_inner::<I, S>(&RefCell::new(flash), flash_range, item, 0);

    fn store_item_inner<'a, 'b, I: StorageItem, S: NorFlash>(
        flash: &'a RefCell<&'b mut S>,
        flash_range: Range<u32>,
        item: I,
        recursion_level: usize,
    ) -> Result<(), MapError<I::Error, S::Error>>
    where
        'a: 'b,
    {
        #[cfg(feature = "defmt")]
        defmt::trace!("Store item inner. Recursion: {}", recursion_level);

        // Check if we're in an infinite recursion which happens when
        if recursion_level == get_pages::<S>(flash_range.clone(), 0).count() {
            return Err(MapError::FullStorage);
        }

        let mut next_page_to_use = None;

        // If there is a partial open page, we try to write in that first if there is enough space
        if let Some(partial_open_page) = {
            let mut flash = flash.borrow_mut(); // Out of band because of weird drop rules
            find_first_page(*flash, flash_range.clone(), 0, PageState::PartialOpen)?
        } {
            #[cfg(feature = "defmt")]
            defmt::trace!("Partial open page found: {}", partial_open_page);

            // We've got to search where the free space is since the page starts with items present already

            let page_data_start_address =
                calculate_page_address::<S>(flash_range.clone(), partial_open_page)
                    + S::WRITE_SIZE as u32;
            let page_data_end_address =
                calculate_page_end_address::<S>(flash_range.clone(), partial_open_page)
                    - S::WRITE_SIZE as u32;

            let mut last_start_address = page_data_start_address;

            for found_item_result in
                read_page_items::<I, S>(flash, flash_range.clone(), partial_open_page)?
            {
                let (_, item_address, item_size) = found_item_result?;
                last_start_address = item_address + item_size as u32;
            }

            let available_bytes_in_page = (page_data_end_address - last_start_address) as usize;

            let mut buffer = [0xFF; MAX_STORAGE_ITEM_SIZE];
            match item
                .serialize_into(&mut buffer[..MAX_STORAGE_ITEM_SIZE.min(available_bytes_in_page)])
            {
                Ok(mut used_bytes) => {
                    // We can only write in whole words, so we round up the used bytes so the math works
                    if used_bytes % S::WRITE_SIZE > 0 {
                        used_bytes += S::WRITE_SIZE - (used_bytes % S::WRITE_SIZE);
                    }

                    flash
                        .borrow_mut()
                        .write(last_start_address, &buffer[..used_bytes])
                        .map_err(MapError::Storage)?;

                    #[cfg(feature = "defmt")]
                    defmt::trace!("Item has been written ok");

                    return Ok(());
                }
                Err(e) if e.is_buffer_too_small() => {
                    #[cfg(feature = "defmt")]
                    defmt::trace!(
                        "Partial open page is too small. Closing it now: {}",
                        partial_open_page
                    );

                    // The item doesn't fit here, so we need to close this page and move to the next
                    close_page(*flash.borrow_mut(), flash_range.clone(), partial_open_page)?;
                    next_page_to_use = Some(next_page::<S>(flash_range.clone(), partial_open_page));
                }
                Err(e) => {
                    return Err(MapError::Item(e));
                }
            }
        }

        // If we get here, there was no partial page found or the partial page has now been closed because the item didn't fit.
        // If there was a partial page, then we need to look at the next page. It's supposed to be open since it was the previous empty buffer page.
        // The new buffer page has to be emptied if it was closed.
        // If there was no partial page, we just use the first open page.

        #[cfg(feature = "defmt")]
        defmt::trace!("Next page to use: {}", next_page_to_use);

        match next_page_to_use {
            Some(next_page_to_use) => {
                let next_page_state =
                    get_page_state(*flash.borrow_mut(), flash_range.clone(), next_page_to_use)?;

                if !next_page_state.is_open() {
                    // What was the previous buffer page was not open...
                    return Err(MapError::Corrupted);
                }

                let next_buffer_page = next_page::<S>(flash_range.clone(), next_page_to_use);
                let next_buffer_page_state =
                    get_page_state(*flash.borrow_mut(), flash_range.clone(), next_buffer_page)?;

                if !next_buffer_page_state.is_open() {
                    // We need to move the data from the next buffer page to the next_page_to_use, but only if that data
                    // doesn't have a newer value somewhere else.

                    let mut next_page_write_address =
                        calculate_page_address::<S>(flash_range.clone(), next_page_to_use)
                            + S::WRITE_SIZE as u32;

                    for old_item in
                        read_page_items::<I, S>(flash, flash_range.clone(), next_buffer_page)?
                    {
                        let (old_item, _, _) = old_item?;

                        let Some((_, newest_version_address, newest_version_len)) =
                            fetch_item_with_location::<I, S>(
                                *flash.borrow_mut(),
                                flash_range.clone(),
                                old_item.key(),
                            )?
                        else {
                            // What do you mean we can't find the item again?
                            return Err(MapError::Corrupted);
                        };

                        let newest_old_item_page =
                            calculate_page_index::<S>(flash_range.clone(), newest_version_address);

                        if newest_old_item_page == next_buffer_page {
                            // The newest version of this item is on the next buffer page, so we need to move it
                            let mut buffer = [0xFF; MAX_STORAGE_ITEM_SIZE];
                            flash
                                .borrow_mut()
                                .read(newest_version_address, &mut buffer[..newest_version_len])
                                .map_err(MapError::Storage)?;

                            // We don't have to watch for the end of the page, because the data we're writing here
                            // is equal or less than the page
                            flash
                                .borrow_mut()
                                .write(next_page_write_address, &buffer[..newest_version_len])
                                .map_err(MapError::Storage)?;

                            next_page_write_address += newest_version_len as u32;
                        }
                    }

                    flash
                        .borrow_mut()
                        .erase(
                            calculate_page_address::<S>(flash_range.clone(), next_buffer_page),
                            calculate_page_end_address::<S>(flash_range.clone(), next_buffer_page),
                        )
                        .map_err(MapError::Storage)?;
                }

                partial_close_page(*flash.borrow_mut(), flash_range.clone(), next_page_to_use)?;
            }
            None => {
                // There's no partial open page, so we just gotta turn the first open page into a partial open one
                let first_open_page = match find_first_page(
                    *flash.borrow_mut(),
                    flash_range.clone(),
                    0,
                    PageState::Open,
                )? {
                    Some(first_open_page) => first_open_page,
                    None => {
                        #[cfg(feature = "defmt")]
                        defmt::error!(
                            "No open pages found for sequential storage in the range: {}",
                            flash_range
                        );
                        // Uh oh, no open pages.
                        // Something has gone wrong.
                        // We should never get here.
                        return Err(MapError::Corrupted);
                    }
                };

                partial_close_page(*flash.borrow_mut(), flash_range.clone(), first_open_page)?;
            }
        }

        // If we get here, we just freshly partially closed a new page, so this should succeed
        store_item_inner::<I, S>(flash, flash_range, item, recursion_level + 1)
    }
}

fn read_page_items<'a, 'b, I: StorageItem, S: NorFlash>(
    flash: &'a RefCell<&'b mut S>,
    flash_range: Range<u32>,
    page_index: usize,
) -> Result<
    impl Iterator<Item = Result<(I, u32, usize), MapError<I::Error, S::Error>>> + 'a,
    MapError<I::Error, S::Error>,
>
where
    'a: 'b,
{
    let mut read_buffer = [0xFF; MAX_STORAGE_ITEM_SIZE];
    let mut used_read_buffer = 0;
    let page_data_start_address =
        calculate_page_address::<S>(flash_range.clone(), page_index) + S::WRITE_SIZE as u32;
    let page_data_end_address =
        calculate_page_end_address::<S>(flash_range.clone(), page_index) - S::WRITE_SIZE as u32;
    let mut read_buffer_start_index_into_page = 0;

    flash
        .borrow_mut()
        .read(
            page_data_start_address,
            &mut read_buffer[used_read_buffer
                ..MAX_STORAGE_ITEM_SIZE
                    .min((page_data_end_address - page_data_start_address) as usize)],
        )
        .map_err(MapError::Storage)?;

    Ok(core::iter::from_fn(move || {
        // Now we deserialize the items from the buffer one by one
        // Every time we proceed, we remove the bytes we used and fill the buffer back up

        macro_rules! replenish_read_buffer {
            () => {{
                read_buffer.copy_within(used_read_buffer.., 0);
                // We need to replenish the used_bytes at the end of the buffer
                let replenish_slice = &mut read_buffer[MAX_STORAGE_ITEM_SIZE - used_read_buffer..];

                let replenish_start_address = page_data_start_address
                    + read_buffer_start_index_into_page as u32
                    + MAX_STORAGE_ITEM_SIZE as u32
                    - used_read_buffer as u32;

                let unread_bytes_left_in_page =
                    page_data_end_address.saturating_sub(replenish_start_address);

                let (read_slice, fill_slice) = replenish_slice
                    .split_at_mut((unread_bytes_left_in_page as usize).min(replenish_slice.len()));

                if !read_slice.is_empty() {
                    if let Err(e) = flash
                        .borrow_mut()
                        .read(replenish_start_address, read_slice)
                        .map_err(MapError::Storage)
                    {
                        return Some(Err(e.into()));
                    }
                }
                fill_slice.fill(0xFF);
                used_read_buffer = 0;
            }};
        }

        if used_read_buffer == read_buffer.len() {
            replenish_read_buffer!();
        }

        if read_buffer[used_read_buffer..].iter().all(|b| *b == 0xFF) {
            // The entire buffer is in the erased state, so we know that the rest is empty
            return None;
        }

        loop {
            match I::deserialize_from(&read_buffer[used_read_buffer..]) {
                Ok((item, mut used_bytes)) => {
                    // We can only write in whole words, so we round up the used bytes so the math works
                    if used_bytes % S::WRITE_SIZE > 0 {
                        used_bytes += S::WRITE_SIZE - (used_bytes % S::WRITE_SIZE);
                    }

                    let return_item = Ok((
                        item,
                        page_data_start_address + read_buffer_start_index_into_page as u32,
                        used_bytes,
                    ));

                    read_buffer_start_index_into_page += used_bytes;
                    used_read_buffer += used_bytes;

                    break Some(return_item);
                }
                Err(e) if e.is_buffer_too_small() && used_read_buffer > 0 => {
                    replenish_read_buffer!();
                }
                Err(e) => {
                    return Some(Err(MapError::Item(e)));
                }
            }
        }
    }))
}

/// A way of serializing and deserializing items in the storage.
///
/// A serialized byte pattern of all `0xFF` is invalid and must never be the result of the `serialize_into` function
/// and `deserialize_from` must always return an error for it.
///
/// The given buffer to serialize in and deserialize from is never bigger than [MAX_STORAGE_ITEM_SIZE] bytes, so make sure the item is
/// smaller than that.
pub trait StorageItem {
    /// The key type of the key-value pair
    type Key: Eq;
    /// The error type for serialization and deserialization
    type Error: StorageItemError;

    /// Serialize the key-value item into the given buffer.
    /// Returns the number of bytes the buffer was filled with or an error.
    ///
    /// The serialized bytes must not all be `0xFF`. One way to prevent this is to serialize an extra 0 byte at the end if that is the case.
    fn serialize_into(&self, buffer: &mut [u8]) -> Result<usize, Self::Error>;
    /// Deserialize the key-value item from the given buffer.
    /// The buffer is likely bigger than the size of the item.
    fn deserialize_from(buffer: &[u8]) -> Result<(Self, usize), Self::Error>
    where
        Self: Sized;

    /// The key of the key-value item. It is used by the storage to know what the key of this item is.
    fn key(&self) -> Self::Key;
}

/// The maximum size in bytes that a storage item can be
pub const MAX_STORAGE_ITEM_SIZE: usize = 512;

/// A trait that the storage item error needs to implement
pub trait StorageItemError: Debug {
    /// Returns true if the error indicates that the buffer is too small to contain the storage item
    fn is_buffer_too_small(&self) -> bool;
}

/// The main error type
#[non_exhaustive]
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum MapError<I, S> {
    /// A storage item error
    Item(I),
    /// An error in the storage (flash)
    Storage(S),
    /// The item cannot be stored anymore because the storage is full.
    /// If you get this error some data may be lost.
    FullStorage,
    /// It's been detected that the memory is likely corrupted.
    /// You may want to erase the memory to recover.
    Corrupted,
    /// A provided buffer was to big to be used
    BufferTooBig,
    /// A provided buffer was to small to be used
    BufferTooSmall,
}

impl<S, I> From<super::Error<S>> for MapError<I, S> {
    fn from(value: super::Error<S>) -> Self {
        match value {
            Error::Storage(e) => Self::Storage(e),
            Error::FullStorage => Self::FullStorage,
            Error::Corrupted => Self::Corrupted,
            Error::BufferTooBig => Self::BufferTooBig,
            Error::BufferTooSmall => Self::BufferTooSmall,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type MockFlashBig = mock_flash::MockFlashBase<4, 4, 256>;
    type MockFlashTiny = mock_flash::MockFlashBase<2, 1, 32>;

    #[derive(Debug, PartialEq, Eq)]
    struct MockStorageItem {
        key: u8,
        value: Vec<u8>,
    }

    #[derive(Debug, PartialEq, Eq)]
    enum MockStorageItemError {
        BufferTooSmall,
        InvalidKey,
        BufferTooBig,
    }

    impl StorageItemError for MockStorageItemError {
        fn is_buffer_too_small(&self) -> bool {
            matches!(self, MockStorageItemError::BufferTooSmall)
        }
    }

    impl StorageItem for MockStorageItem {
        type Key = u8;

        type Error = MockStorageItemError;

        fn serialize_into(&self, buffer: &mut [u8]) -> Result<usize, Self::Error> {
            if buffer.len() < 2 + self.value.len() {
                return Err(MockStorageItemError::BufferTooSmall);
            }

            if self.value.len() > 255 {
                return Err(MockStorageItemError::BufferTooBig);
            }

            // The serialized value must not be all 0xFF
            if self.key == 0xFF {
                return Err(MockStorageItemError::InvalidKey);
            }

            buffer[0] = self.key;
            buffer[1] = self.value.len() as u8;
            buffer[2..][..self.value.len()].copy_from_slice(&self.value);

            Ok(2 + self.value.len())
        }

        fn deserialize_from(buffer: &[u8]) -> Result<(Self, usize), Self::Error>
        where
            Self: Sized,
        {
            if buffer.len() < 2 {
                return Err(MockStorageItemError::BufferTooSmall);
            }

            if buffer[0] == 0xFF {
                return Err(MockStorageItemError::InvalidKey);
            }

            let len = buffer[1];

            if buffer.len() < 2 + len as usize {
                return Err(MockStorageItemError::BufferTooSmall);
            }

            Ok((
                Self {
                    key: buffer[0],
                    value: buffer[2..][..len as usize].to_vec(),
                },
                2 + len as usize,
            ))
        }

        fn key(&self) -> Self::Key {
            self.key
        }
    }

    #[test]
    fn store_and_fetch() {
        let mut flash = MockFlashBig::new();
        let flash_range = 0x000..0x1000;

        let item = fetch_item::<MockStorageItem, _>(&mut flash, flash_range.clone(), 0).unwrap();
        assert_eq!(item, None);

        let item = fetch_item::<MockStorageItem, _>(&mut flash, flash_range.clone(), 60).unwrap();
        assert_eq!(item, None);

        let item = fetch_item::<MockStorageItem, _>(&mut flash, flash_range.clone(), 0xFF).unwrap();
        assert_eq!(item, None);

        store_item::<_, _>(
            &mut flash,
            flash_range.clone(),
            MockStorageItem {
                key: 0,
                value: vec![5],
            },
        )
        .unwrap();
        store_item::<_, _>(
            &mut flash,
            flash_range.clone(),
            MockStorageItem {
                key: 0,
                value: vec![5, 6],
            },
        )
        .unwrap();

        let item = fetch_item::<MockStorageItem, _>(&mut flash, flash_range.clone(), 0)
            .unwrap()
            .unwrap();
        assert_eq!(item.key, 0);
        assert_eq!(item.value, vec![5, 6]);

        store_item::<_, _>(
            &mut flash,
            flash_range.clone(),
            MockStorageItem {
                key: 1,
                value: vec![2, 2, 2, 2, 2, 2],
            },
        )
        .unwrap();

        let item = fetch_item::<MockStorageItem, _>(&mut flash, flash_range.clone(), 0)
            .unwrap()
            .unwrap();
        assert_eq!(item.key, 0);
        assert_eq!(item.value, vec![5, 6]);

        let item = fetch_item::<MockStorageItem, _>(&mut flash, flash_range.clone(), 1)
            .unwrap()
            .unwrap();
        assert_eq!(item.key, 1);
        assert_eq!(item.value, vec![2, 2, 2, 2, 2, 2]);

        for index in 0..4000 {
            store_item::<_, _>(
                &mut flash,
                flash_range.clone(),
                MockStorageItem {
                    key: (index % 10) as u8,
                    value: vec![(index % 10) as u8 * 2; index % 10],
                },
            )
            .unwrap();
        }

        for i in 0..10 {
            let item = fetch_item::<MockStorageItem, _>(&mut flash, flash_range.clone(), i)
                .unwrap()
                .unwrap();
            assert_eq!(item.key, i);
            assert_eq!(item.value, vec![(i % 10) as u8 * 2; (i % 10) as usize]);
        }

        for _ in 0..4000 {
            store_item::<_, _>(
                &mut flash,
                flash_range.clone(),
                MockStorageItem {
                    key: 11,
                    value: vec![0; 10],
                },
            )
            .unwrap();
        }

        for i in 0..10 {
            let item = fetch_item::<MockStorageItem, _>(&mut flash, flash_range.clone(), i)
                .unwrap()
                .unwrap();
            assert_eq!(item.key, i);
            assert_eq!(item.value, vec![(i % 10) as u8 * 2; (i % 10) as usize]);
        }

        println!(
            "Erases: {}, reads: {}, writes: {}",
            flash.erases, flash.reads, flash.writes
        );
    }

    #[test]
    fn store_too_many_items() {
        const UPPER_BOUND: u8 = 6;

        let mut tiny_flash = MockFlashTiny::new();

        for i in 0..UPPER_BOUND {
            let item = MockStorageItem {
                key: i as u8,
                value: vec![i as u8; i as usize],
            };
            println!("Storing {item:?}");

            store_item::<_, _>(&mut tiny_flash, 0x00..0x40, item).unwrap();
        }

        assert_eq!(
            store_item::<_, _>(
                &mut tiny_flash,
                0x00..0x40,
                MockStorageItem {
                    key: UPPER_BOUND,
                    value: vec![0; UPPER_BOUND as usize],
                },
            ),
            Err(MapError::FullStorage)
        );

        for i in 0..UPPER_BOUND {
            let item = fetch_item::<MockStorageItem, _>(&mut tiny_flash, 0x00..0x40, i as u8)
                .unwrap()
                .unwrap();

            println!("Fetched {item:?}");

            assert_eq!(item.value, vec![i as u8; i as usize]);
        }
    }

    #[test]
    fn store_too_many_items_big() {
        const UPPER_BOUND: u8 = 74;

        let mut big_flash = MockFlashBig::new();

        for i in 0..UPPER_BOUND {
            let item = MockStorageItem {
                key: i as u8,
                value: vec![i as u8; i as usize],
            };
            println!("Storing {item:?}");

            store_item::<_, _>(&mut big_flash, 0x0000..0x1000, item).unwrap();
        }

        assert_eq!(
            store_item::<_, _>(
                &mut big_flash,
                0x0000..0x1000,
                MockStorageItem {
                    key: UPPER_BOUND,
                    value: vec![0; UPPER_BOUND as usize],
                },
            ),
            Err(MapError::FullStorage)
        );

        for i in 0..UPPER_BOUND {
            let item = fetch_item::<MockStorageItem, _>(&mut big_flash, 0x0000..0x1000, i as u8)
                .unwrap()
                .unwrap();

            println!("Fetched {item:?}");

            assert_eq!(item.value, vec![i as u8; i as usize]);
        }
    }

    #[test]
    fn store_many_items_big() {
        let mut flash = mock_flash::MockFlashBase::<4, 1, 4096>::new();

        const LENGHT_PER_KEY: [usize; 24] = [
            11, 13, 6, 13, 13, 10, 2, 3, 5, 36, 1, 65, 4, 6, 1, 15, 10, 7, 3, 15, 9, 3, 4, 5,
        ];

        for _ in 0..1000 {
            for i in 0..24 {
                let item = MockStorageItem {
                    key: i as u8,
                    value: vec![i as u8; LENGHT_PER_KEY[i]],
                };

                store_item::<_, _>(&mut flash, 0x0000..0x4000, item).unwrap();
            }
        }

        for i in 0..24 {
            let item = fetch_item::<MockStorageItem, _>(&mut flash, 0x0000..0x4000, i as u8)
                .unwrap()
                .unwrap();

            println!("Fetched {item:?}");

            assert_eq!(item.value, vec![i as u8; LENGHT_PER_KEY[i]]);
        }
    }
}
