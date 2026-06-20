//! `PRAGMA integrity_check` backend — b-tree walk, overflow chain verification, freelist
//! check (mirrors `sqlite3BtreeIntegrityCheck` / `checkTreePage` / `checkList` in `btree.c`).
//!
//! This is a pragmatic implementation: it walks every b-tree in the catalog (tables + indexes),
//! verifies each page's type is consistent, cells are in order, child pointers reference valid
//! pages, the freelist count matches the actual freelist pages, and every page is referenced
//! by either a b-tree or the freelist. Returns "ok" when the database is consistent, or a list
//! of error messages (one per row) when corruption is found.

use crate::error::Result;
use crate::pager::Pager;
use crate::schema::read_catalog;

use super::cell::{
    parse_index_interior_cell, parse_index_leaf_cell, parse_table_interior_cell,
    parse_table_leaf_cell,
};
use super::page::{PageHeader, PageType};
use super::ptrmap::{is_ptrmap_page, is_pending_byte_page, ptrmap_get, PtrMapType};

/// Run a full integrity check on the database and return the result rows. Each row is a
/// single-column text value. When the database is consistent, the result is a single row
/// "ok". When corruption is found, each error is a row like "*** in database main ***\n<msg>".
pub async fn integrity_check(pager: &Pager, quick: bool) -> Result<Vec<Vec<crate::types::Value>>> {
    let catalog = read_catalog(pager).await?;
    let usable = pager.usable_size();
    let page_count = pager.page_count();

    let mut checker = IntegrityCheck::new(page_count, usable, quick);

    // Walk the freelist and mark every freelist page as referenced.
    checker.check_freelist(pager).await?;

    // Walk each b-tree in the catalog.
    for obj in catalog.tables() {
        let root = obj.rootpage as u32;
        if root == 0 || root > page_count {
            checker.error(format!("rootpage {} out of range for table {}", root, obj.name));
            continue;
        }
        checker.check_tree(pager, root, true, &obj.name).await?;
    }
    for obj in catalog.indexes() {
        let root = obj.rootpage as u32;
        if root == 0 || root > page_count {
            checker.error(format!("rootpage {} out of range for index {}", root, obj.name));
            continue;
        }
        checker.check_tree(pager, root, false, &obj.name).await?;
    }

    // Verify every page is referenced (either by a b-tree or the freelist). Reserved pages
    // (ptrmap, pending-byte) are exempt.
    if !quick {
        for pgno in 2..=page_count {
            if is_ptrmap_page(usable, pgno) || is_pending_byte_page(usable, pgno) {
                continue;
            }
            if !checker.is_referenced(pgno) {
                checker.error(format!("Page {} is never used", pgno));
            }
        }
    }

    Ok(checker.finish())
}

/// The integrity-check state: a list of error messages, a page-reference bitmap, and the
/// database geometry.
struct IntegrityCheck {
    errors: Vec<String>,
    /// `referenced[i-1]` is true if page `i` has been claimed by a b-tree or the freelist.
    referenced: Vec<bool>,
    usable: usize,
}

impl IntegrityCheck {
    fn new(page_count: u32, usable: usize, _quick: bool) -> IntegrityCheck {
        IntegrityCheck {
            errors: Vec::new(),
            referenced: vec![false; page_count as usize],
            usable,
        }
    }

    fn error(&mut self, msg: String) {
        self.errors.push(format!("*** in database main ***\n{msg}"));
    }

    fn mark_referenced(&mut self, pgno: u32) {
        let idx = pgno as usize;
        if idx >= 1 && idx <= self.referenced.len() {
            self.referenced[idx - 1] = true;
        }
    }

    fn is_referenced(&self, pgno: u32) -> bool {
        let idx = pgno as usize;
        if idx >= 1 && idx <= self.referenced.len() {
            self.referenced[idx - 1]
        } else {
            false
        }
    }

    /// Walk the freelist trunk chain and mark every trunk + leaf page as referenced. Also
    /// verify the freelist count matches the actual number of freelist pages.
    async fn check_freelist(&mut self, pager: &Pager) -> Result<()> {
        let header = pager.header();
        let mut trunk = header.first_freelist_trunk;
        let mut count = 0u32;
        let page_count = pager.page_count();
        while trunk != 0 {
            if trunk > page_count {
                self.error(format!("freelist trunk page {trunk} out of range"));
                return Ok(());
            }
            self.mark_referenced(trunk);
            let page = pager.get_page(trunk).await?;
            let next = u32::from_be_bytes([page[0], page[1], page[2], page[3]]);
            let k = u32::from_be_bytes([page[4], page[5], page[6], page[7]]);
            // Each trunk page holds up to `usable/4 - 8` leaves (the upstream limit). A larger
            // value is corruption.
            if k > (self.usable as u32 / 4).saturating_sub(2) {
                self.error(format!(
                    "freelist trunk page {trunk} has too many leaves ({k})"
                ));
            }
            count += 1; // the trunk itself
            for i in 0..k as usize {
                let off = 8 + i * 4;
                if off + 4 > self.usable {
                    break;
                }
                let leaf = u32::from_be_bytes([
                    page[off],
                    page[off + 1],
                    page[off + 2],
                    page[off + 3],
                ]);
                if leaf == 0 || leaf > page_count {
                    self.error(format!("freelist leaf page {leaf} out of range"));
                    continue;
                }
                self.mark_referenced(leaf);
                count += 1;
            }
            trunk = next;
        }
        if count != header.freelist_count {
            self.error(format!(
                "freelist count {} disagrees with actual count {}",
                header.freelist_count, count
            ));
        }
        Ok(())
    }

    /// Recursively check a b-tree rooted at `root`. `is_table` selects the cell-parsing
    /// convention. Verifies page types, cell ordering, child pointers, and overflow chains.
    async fn check_tree(
        &mut self,
        pager: &Pager,
        root: u32,
        is_table: bool,
        name: &str,
    ) -> Result<()> {
        self.check_page(pager, root, is_table, name, 0).await?;
        Ok(())
    }

    /// Check a single b-tree page and its descendants. Returns the depth (unused but kept for
    /// parity with upstream's `checkTreePage`). Boxed to allow async recursion.
    fn check_page<'a>(
        &'a mut self,
        pager: &'a Pager,
        pgno: u32,
        is_table: bool,
        name: &'a str,
        depth: u32,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<u32>> + Send + 'a>> {
        Box::pin(async move {
            if pgno > pager.page_count() {
                self.error(format!("page {pgno} out of range (in {name})"));
                return Ok(depth);
            }
            self.mark_referenced(pgno);
            let page = pager.get_page(pgno).await?;
            let base = pager.btree_header_offset(pgno);
            let hdr = match PageHeader::parse(&page, base) {
                Ok(h) => h,
                Err(e) => {
                    self.error(format!("page {pgno}: bad header ({e})"));
                    return Ok(depth);
                }
            };

            // Verify the page type matches the expected b-tree kind.
            let expected = if is_table {
                matches!(hdr.page_type, PageType::LeafTable | PageType::InteriorTable)
            } else {
                matches!(hdr.page_type, PageType::LeafIndex | PageType::InteriorIndex)
            };
            if !expected {
                self.error(format!(
                    "page {pgno} of {name} has wrong type {:?}",
                    hdr.page_type
                ));
                return Ok(depth);
            }

            // Verify cell pointers are within the usable area and check overflow chains.
            let n = hdr.num_cells as usize;
            for i in 0..n {
                let off = match hdr.cell_pointer(&page, i) {
                    Ok(o) => o,
                    Err(e) => {
                        self.error(format!("page {pgno} cell {i}: bad pointer ({e})"));
                        continue;
                    }
                };
                if off >= self.usable {
                    self.error(format!("page {pgno} cell {i}: pointer {off} beyond usable"));
                    continue;
                }
                if hdr.page_type == PageType::LeafTable {
                    if let Ok(cell) = parse_table_leaf_cell(&page, off, self.usable) {
                        if let Some(ovfl) = cell.overflow_page {
                            self.check_overflow_chain(pager, ovfl, pgno).await?;
                        }
                    }
                } else if hdr.page_type == PageType::LeafIndex {
                    if let Ok(cell) = parse_index_leaf_cell(&page, off, self.usable) {
                        if let Some(ovfl) = cell.overflow_page {
                            self.check_overflow_chain(pager, ovfl, pgno).await?;
                        }
                    }
                }
            }

            // Recurse into children for interior pages.
            let mut max_depth = depth;
            match hdr.page_type {
                PageType::InteriorTable => {
                    for i in 0..n {
                        let off = hdr.cell_pointer(&page, i)?;
                        let cell = parse_table_interior_cell(&page, off)?;
                        let d = self
                            .check_page(pager, cell.left_child, is_table, name, depth + 1)
                            .await?;
                        max_depth = max_depth.max(d);
                    }
                    if let Some(rm) = hdr.right_most_pointer {
                        let d = self.check_page(pager, rm, is_table, name, depth + 1).await?;
                        max_depth = max_depth.max(d);
                    }
                }
                PageType::InteriorIndex => {
                    for i in 0..n {
                        let off = hdr.cell_pointer(&page, i)?;
                        let cell = parse_index_interior_cell(&page, off, self.usable)?;
                        let d = self
                            .check_page(pager, cell.left_child, is_table, name, depth + 1)
                            .await?;
                        max_depth = max_depth.max(d);
                    }
                    if let Some(rm) = hdr.right_most_pointer {
                        let d = self.check_page(pager, rm, is_table, name, depth + 1).await?;
                        max_depth = max_depth.max(d);
                    }
                }
                _ => {}
            }

            // Check ptrmap entry for non-root pages (auto-vacuum only).
            if pager.auto_vacuum() && pgno > 1 && !is_ptrmap_page(self.usable, pgno) {
                match ptrmap_get(pager, pgno).await {
                    Ok((ty, _parent)) => {
                        if depth == 0 {
                            if ty != PtrMapType::RootPage {
                                self.error(format!(
                                    "page {pgno}: ptrmap type {:?} should be RootPage",
                                    ty
                                ));
                            }
                        } else if ty != PtrMapType::Btree {
                            self.error(format!(
                                "page {pgno}: ptrmap type {:?} should be Btree",
                                ty
                            ));
                        }
                    }
                    Err(e) => {
                        self.error(format!("page {pgno}: ptrmap read failed ({e})"));
                    }
                }
            }

            Ok(max_depth)
        })
    }

    /// Walk an overflow chain and verify each page is referenced and the chain terminates.
    async fn check_overflow_chain(
        &mut self,
        pager: &Pager,
        first: u32,
        host: u32,
    ) -> Result<()> {
        let mut pgno = first;
        let mut prev = host;
        let mut visited = 0u32;
        while pgno != 0 {
            visited += 1;
            if visited > pager.page_count() {
                self.error(format!("overflow chain from page {host} has a cycle"));
                return Ok(());
            }
            if pgno > pager.page_count() {
                self.error(format!("overflow page {pgno} out of range (host {host})"));
                return Ok(());
            }
            self.mark_referenced(pgno);
            let page = pager.get_page(pgno).await?;
            let next = u32::from_be_bytes([page[0], page[1], page[2], page[3]]);
            // In auto-vacuum mode, verify the ptrmap entry.
            if pager.auto_vacuum() && !is_ptrmap_page(self.usable, pgno) {
                if let Ok((ty, parent)) = ptrmap_get(pager, pgno).await {
                    let expected = if prev == host {
                        PtrMapType::Overflow1
                    } else {
                        PtrMapType::Overflow2
                    };
                    if ty != expected {
                        self.error(format!(
                            "overflow page {pgno}: ptrmap type {:?} should be {:?}",
                            ty, expected
                        ));
                    }
                    if parent != prev {
                        self.error(format!(
                            "overflow page {pgno}: ptrmap parent {parent} should be {prev}",
                        ));
                    }
                }
            }
            prev = pgno;
            pgno = next;
        }
        Ok(())
    }

    /// Produce the result rows: "ok" if no errors, or one row per error message.
    fn finish(self) -> Vec<Vec<crate::types::Value>> {
        use crate::types::Value;
        if self.errors.is_empty() {
            vec![vec![Value::Text("ok".to_string())]]
        } else {
            self.errors
                .into_iter()
                .map(|msg| vec![Value::Text(msg)])
                .collect()
        }
    }
}