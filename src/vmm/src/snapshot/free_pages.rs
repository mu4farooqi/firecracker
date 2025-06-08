// Copyright 2024 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Free pages bitmap management for snapshot memory zeroing optimization.
//!
//! This module provides functionality to track pages that are inflated by the
//! balloon device and should be considered free from the guest perspective.
//! This information is used to zero free pages in memory files for better compression.

use std::collections::HashSet;

/// Error type for free pages bitmap operations.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum FreePagesError {
    /// Invalid page frame number: {0} exceeds total pages {1}
    InvalidPageNumber(u32, u64),
}

/// Memory-efficient bitmap for tracking free pages using bit vectors.
#[derive(Debug)]
pub struct FreePagesbitmap {
    /// Bitmap where each bit represents whether a page is free (1) or not (0).
    /// Much more memory efficient than storing individual page numbers.
    pub bitmap: Vec<u8>,
    /// Total number of memory pages in the guest.
    pub total_pages: u64,
    /// Page size in bytes (typically 4096).
    pub page_size: u32,
}

impl FreePagesbitmap {
    /// Creates a new free pages bitmap from a set of page numbers.
    pub fn new(
        free_pages: &HashSet<u32>,
        total_pages: u64,
        page_size: u32,
    ) -> Result<Self, FreePagesError> {
        // Calculate bitmap size in bytes (1 bit per page, rounded up to byte boundary)
        let bitmap_bytes = ((total_pages + 7) / 8) as usize;
        let mut bitmap = vec![0u8; bitmap_bytes];

        // Set bits for free pages
        for &page_num in free_pages {
            if page_num as u64 >= total_pages {
                return Err(FreePagesError::InvalidPageNumber(page_num, total_pages));
            }
            let byte_idx = (page_num / 8) as usize;
            let bit_idx = page_num % 8;
            bitmap[byte_idx] |= 1 << bit_idx;
        }

        Ok(Self {
            bitmap,
            total_pages,
            page_size,
        })
    }

    /// Returns the number of free pages by counting set bits.
    pub fn free_page_count(&self) -> usize {
        self.bitmap.iter().map(|byte| byte.count_ones() as usize).sum()
    }

    /// Checks if a page is marked as free.
    pub fn is_page_free(&self, page_frame_number: u32) -> bool {
        if page_frame_number as u64 >= self.total_pages {
            return false;
        }
        let byte_idx = (page_frame_number / 8) as usize;
        let bit_idx = page_frame_number % 8;
        (self.bitmap[byte_idx] & (1 << bit_idx)) != 0
    }
}

/// Creates a free pages bitmap from balloon device inflated pages.
/// This is used for memory zeroing optimization during snapshot creation.
pub fn create_free_pages_bitmap(
    inflated_pages: &HashSet<u32>,
    total_memory_pages: u64,
    page_size: u32,
) -> Result<FreePagesbitmap, FreePagesError> {
    FreePagesbitmap::new(inflated_pages, total_memory_pages, page_size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bitmap_operations() {
        let mut free_pages = HashSet::new();
        free_pages.insert(100);
        free_pages.insert(200);
        free_pages.insert(300);

        let bitmap = FreePagesbitmap::new(&free_pages, 1000, 4096).unwrap();

        // Test is_page_free
        assert!(bitmap.is_page_free(100));
        assert!(bitmap.is_page_free(200));
        assert!(bitmap.is_page_free(300));
        assert!(!bitmap.is_page_free(150));

        // Test free_page_count
        assert_eq!(bitmap.free_page_count(), 3);
    }

    #[test]
    fn test_bitmap_memory_efficiency() {
        // Test that bitmap uses minimal memory
        let total_pages = 1_000_000u64; // ~4GB worth of pages
        let free_pages = HashSet::new(); // Empty set
        let bitmap = FreePagesbitmap::new(&free_pages, total_pages, 4096).unwrap();

        // Bitmap should use about 125KB (1M bits / 8 bits per byte)
        let expected_bitmap_bytes = ((total_pages + 7) / 8) as usize;
        assert_eq!(bitmap.bitmap.len(), expected_bitmap_bytes);
    }

    #[test]
    fn test_create_free_pages_bitmap() {
        let mut inflated_pages = HashSet::new();
        inflated_pages.insert(42);
        inflated_pages.insert(1337);

        let bitmap = create_free_pages_bitmap(&inflated_pages, 10000, 4096).unwrap();
        assert!(bitmap.is_page_free(42));
        assert!(bitmap.is_page_free(1337));
        assert!(!bitmap.is_page_free(500));
        assert_eq!(bitmap.free_page_count(), 2);
    }
}
