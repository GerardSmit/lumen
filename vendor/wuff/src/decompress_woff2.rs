use alloc::{boxed::Box, vec::Vec};
use core::error::Error;

use crate::Tag;
use bytes::{Buf as _, BufMut};

use crate::{
    GLYF, HEAD, HMTX, LOCA, MAX_WOFF2_OUTPUT_SIZE, Round4, compute_checksum,
    error::{WuffErr, bail, bail_if, bail_with_msg_if},
    limits::{aligned_len, checked_end, reserve, vec_with_capacity},
    woff::{
        glyf_decoder::tranform_glyf_table,
        headers::{
            CollectionDirectory, CollectionDirectoryEntry, TableDirectory, TableDirectoryEntry,
            WOFF2FontInfo, WoffHeader, WoffVersion,
        },
        hmtx_decoder::{decode_hmtx_table, generate_hmtx_table},
    },
    write_table_directory_header,
};

// Over 14k test fonts the max compression ratio seen to date was ~20.
// >100 suggests you wrote a bad uncompressed size.
const K_MAX_PLAUSIBLE_COMPRESSION_RATIO: f32 = 100.0;

#[allow(clippy::type_complexity)]
/// Decompress a WOFF2 file using a custom brotli decompressor passed as a closure
pub fn decompress_woff2_with_custom_brotli(
    raw_woff_data: &[u8],
    decompress_brotli: &mut dyn FnMut(&[u8], usize) -> Result<Vec<u8>, Box<dyn Error>>,
) -> Result<Vec<u8>, WuffErr> {
    decompress_woff2_with_custom_brotli_limited(
        raw_woff_data,
        MAX_WOFF2_OUTPUT_SIZE,
        decompress_brotli,
    )
}

/// Decompress WOFF2 while bounding decoded table data, reconstruction scratch, and
/// the returned SFNT buffer to `max_output_size` bytes.
///
/// `origLength` and `totalSfntSize` are treated only as format metadata or capacity hints;
/// every decoder-owned allocation and append is independently checked against this explicit
/// limit. The caller-supplied Brotli callback also receives the bounded expected output size
/// and must enforce that bound on its own internal allocations.
#[allow(clippy::type_complexity)]
pub fn decompress_woff2_with_custom_brotli_limited(
    raw_woff_data: &[u8],
    max_output_size: usize,
    decompress_brotli: &mut dyn FnMut(&[u8], usize) -> Result<Vec<u8>, Box<dyn Error>>,
) -> Result<Vec<u8>, WuffErr> {
    // SFNT table offsets and lengths are 32-bit fields. Keep the caller's ceiling
    // within that representable range; the legacy API still uses the 128 MiB default.
    bail_if!(max_output_size == 0 || max_output_size > u32::MAX as usize);

    // Here we create a new view over the `raw_woff_data`. Because we pass `&mut input` to parsing functons,
    // they will actually mutate the slice (not the data it points to) such that it only includes unparsed data.
    //
    // However `raw_woff_data` will still contain the full data for the WOFF.
    let mut input = raw_woff_data;
    let full_input_len = input.len();

    // Parse header, table directory and collection directory
    let header = WoffHeader::parse(&mut input)?;
    bail_if!(header.woff_version != WoffVersion::Woff2);

    let table_directory = TableDirectory::parse_woff2_limited(
        &mut input,
        header.num_tables as usize,
        max_output_size,
    )?;
    let mut collection_directory = if header.is_collection() {
        CollectionDirectory::parse_limited(&mut input, &table_directory, max_output_size)?
    } else {
        CollectionDirectory::generate_for_single_font_limited(
            header.flavor,
            &table_directory,
            max_output_size,
        )?
    };

    // Validate header (blocks do not overlap, and have at most 3 bytes padding between them)

    let compressed_offset = full_input_len - input.len();
    bail_if!(compressed_offset > u32::MAX as usize);

    let mut src_offset = Round4!(compressed_offset + header.total_compressed_size as usize);
    bail_if!(src_offset > full_input_len);

    if header.meta_offset != 0 {
        bail_if!(src_offset != header.meta_offset as usize);
        src_offset = Round4!(header.meta_offset as usize + header.meta_length as usize);
        bail_if!(src_offset > u32::MAX as usize);
    }

    if header.priv_offset != 0 {
        bail_if!(src_offset != header.priv_offset as usize);
        src_offset = Round4!(header.priv_offset as usize + header.priv_length as usize);
        bail_if!(src_offset > u32::MAX as usize);
    }

    bail_if!(src_offset != Round4!(full_input_len));

    // Re-order tables in output (OTSpec) order
    collection_directory.sort_tables_within_each_font(&table_directory);
    let num_fonts = collection_directory.fonts.len();

    // Compute compression ratio using the trusted, table-directory-derived uncompressed size
    // (not the untrusted `totalSfntSize` from the file header). Perform the plausibility check
    // BEFORE decompressing so an implausible size never drives allocation/decompression.
    let compression_ratio: f32 =
        (table_directory.uncompressed_size as f32) / (raw_woff_data.len() as f32);

    // Validate header (and compression ratio)
    bail_if!(header.total_sfnt_size < 1);
    bail_with_msg_if!(
        compression_ratio > K_MAX_PLAUSIBLE_COMPRESSION_RATIO,
        "Implausible compression ratio {:.1}",
        compression_ratio
    );
    bail_if!(table_directory.uncompressed_size > max_output_size);

    // Decompress data with brotli decoder. We pass the trusted `uncompressed_size` as the hard
    // upper bound on the size of the decompressed data.
    let compressed_data = &input[0..(header.total_compressed_size as usize)];
    let decompressed_data = decompress_brotli(compressed_data, table_directory.uncompressed_size)
        .map_err(|_| WuffErr::GenericError)?;

    // The decompressed data block must be exactly the size of the tables it contains
    // (tables are stored consecutively with no padding or extraneous data).
    // <https://www.w3.org/TR/WOFF2/#conform-mustRejectExtraData>
    bail_if!(decompressed_data.len() != table_directory.uncompressed_size);

    // Output capacity hint. `totalSfntSize` is exact for a well-formed font but untrusted, so
    // only use it if it is bounded by the directory-derived size (headers + padded `origLength`s,
    // a slight over-estimate for conformant fonts), falling back to that size otherwise. Both are
    // clamped to the compression-ratio limit and caller limit since `origLength` is also
    // attacker-controlled.
    let output_header_size = collection_directory
        .table_directories_required_size_limited(header.is_collection(), max_output_size)?;
    let expected_sfnt_size: u64 = output_header_size as u64
        + table_directory
            .iter()
            .map(|table| Round4!(table.orig_length as u64))
            .sum::<u64>();
    let total_sfnt_size = header.total_sfnt_size as u64;
    let sfnt_size_is_plausible = total_sfnt_size <= expected_sfnt_size
        && total_sfnt_size >= table_directory.uncompressed_size as u64;
    let max_plausible_size =
        (K_MAX_PLAUSIBLE_COMPRESSION_RATIO as u64).saturating_mul(raw_woff_data.len() as u64);
    let capacity_hint = if sfnt_size_is_plausible {
        total_sfnt_size
    } else {
        expected_sfnt_size
    }
    .min(max_plausible_size)
    .min(max_output_size as u64) as usize;
    let mut out = vec_with_capacity(capacity_hint, max_output_size)?;

    let mut out_header = generate_header(
        &header,
        &table_directory,
        &collection_directory,
        max_output_size,
    )?;
    append_bytes(&mut out, &out_header.data, max_output_size)?;

    // Metadata for tables that have been written. Index corresponds to the table's index within the tables Vec
    let mut table_metadata = vec_with_capacity(header.num_tables as usize, max_output_size)?;
    table_metadata.resize(header.num_tables as usize, None);
    for i in 0..num_fonts {
        reconstruct_font(
            &decompressed_data,
            &header,
            &table_directory,
            &collection_directory.fonts[i],
            &mut out_header,
            &mut table_metadata,
            &mut out,
            i,
            max_output_size,
        )?;
    }

    bail_if!(out.len() > max_output_size);

    // Update header
    out[0..out_header.data.len()].copy_from_slice(&out_header.data);

    // The hint may over-estimate (the directory-derived fallback is a slight over-estimate, and an
    // inflated `origLength` can make it a large one), or the buffer may have outgrown it (leaving
    // up to ~2x excess). Callers tend to retain the buffer, so shrink it if the excess is
    // significant.
    const SHRINK_SLACK: usize = 1024;
    if out.capacity() > out.len() + out.len() / 16 + SHRINK_SLACK {
        out.shrink_to_fit();
    }

    Ok(out)
}

fn iter_tables_for_font<'a>(
    font_entry: &'a CollectionDirectoryEntry,
    tables: &'a TableDirectory,
) -> impl Iterator<Item = (usize, &'a TableDirectoryEntry)> {
    font_entry
        .table_indices
        .iter()
        .map(|table_idx| (*table_idx as usize, &tables[*table_idx as usize]))
}

fn append_bytes(out: &mut Vec<u8>, bytes: &[u8], max_bytes: usize) -> Result<(), WuffErr> {
    checked_end(out.len(), bytes.len(), max_bytes)?;
    reserve(out, bytes.len(), max_bytes)?;
    out.extend_from_slice(bytes);
    Ok(())
}

fn append_padded(out: &mut Vec<u8>, bytes: &[u8], max_bytes: usize) -> Result<(), WuffErr> {
    let end = checked_end(out.len(), bytes.len(), max_bytes)?;
    let padded_end = aligned_len(end, 4, max_bytes)?;
    reserve(out, padded_end - out.len(), max_bytes)?;
    out.extend_from_slice(bytes);
    out.resize(padded_end, 0);
    Ok(())
}

// Offset tables assumed to have been written in with 0's initially.
// WOFF2Header isn't const so we can use [] instead of at() (which upsets FF)
#[allow(clippy::too_many_arguments)]
fn reconstruct_font(
    woff_data: &[u8],
    header: &WoffHeader,
    tables: &TableDirectory,
    font_entry: &CollectionDirectoryEntry,
    out_header: &mut HeaderData,
    table_metadata: &mut [Option<TableMetadata>],
    out: &mut Vec<u8>,
    font_idx: usize,
    max_output_size: usize,
) -> Result<(), WuffErr> {
    let glyf_idx = font_entry.glyf_idx.map(|idx| idx as usize);
    let loca_idx = font_entry.loca_idx.map(|idx| idx as usize);
    let hhea_idx = font_entry.hhea_idx.map(|idx| idx as usize);

    // Check the glyf and loca tables are compatible with each other
    // 'glyf' without 'loca' doesn't make sense
    match (glyf_idx, loca_idx) {
        (Some(glyf_idx), Some(loca_idx)) => {
            bail_with_msg_if!(
                tables[glyf_idx].is_transformed() != tables[loca_idx].is_transformed(),
                "Cannot transform just one of glyf/loca"
            );
        }
        (Some(_), None) | (None, Some(_)) => {
            bail_with_msg_if!(true, "Cannot have just one of glyf/loca")
        }
        (None, None) => {}
    }

    let mut font_checksum: u32 = if header.is_collection() {
        out_header.font_infos[font_idx].header_checksum
    } else {
        out_header.checksum
    };

    // Read and store "num_hmetrics" from "hhea" table and then used to reconstruct "hmtx"
    let num_hmetrics = match hhea_idx {
        Some(hhea_idx) => {
            let hhea_table = &tables[hhea_idx];
            Some(read_num_hmetrics(hhea_table.data_as_slice(woff_data)?)?)
        }
        None => None,
    };

    // These are read from "glyf" and then used to reconstruct "hmtx"
    let mut num_glyphs = None;
    let mut x_mins = None;

    // Iterate over the tables for this font.
    // Note: tables within each font (what we are iterating over here) have already been sorted in alphabetical table tag order.
    for (table_idx, table) in iter_tables_for_font(font_entry, tables) {
        // TODO(user) a collection with optimized hmtx that reused glyf/loca
        // would fail. We don't optimize hmtx for collections yet.
        bail_if!(table.woff_offset as usize + table.woff_length as usize > woff_data.len());

        // Check to see if we have already processed and saved metadata for this table.
        // If we have then
        // There are two cases when this occurs:
        //   - When a table is reused between fonts in a collection (and this table has already been processed for an earlier font)
        //   - For the "loca" table. This table gets processed as part of processing "glyf"
        let metadata = if let Some(metadata) = table_metadata[table_idx] {
            // Tables shouldn't be reused within a single font (they should be reused between different
            // fonts in a collection). So if we encounter a table we have already computed metadata for in the first
            // font unless the table is a "loca" table because we compute metadata for this table when processing the "glyf"
            // table (so for "loca" encountering already-computed metadata doesn't necessarily indicate reuse).
            bail_if!(font_idx == 0 && table.tag != LOCA);

            metadata
        }
        // Any table which does not need to be transformed
        else if !table.is_transformed() {
            let check_sum_adjustment = if table.tag == HEAD {
                bail_if!(table.woff_length < 12);
                let checksum_slice =
                    &woff_data[(table.woff_offset as usize + 8)..(table.woff_offset as usize + 12)];
                let checksum_bytes: [u8; 4] = checksum_slice.try_into().unwrap();
                u32::from_be_bytes(checksum_bytes)
            } else {
                0
            };

            let table_data = table.data_as_slice(woff_data)?;
            let checksum = compute_checksum(table_data).wrapping_sub(check_sum_adjustment);

            let metadata = TableMetadata {
                dst_offset: out.len() as u32,
                dst_length: table.woff_length,
                checksum,
            };
            table_metadata[table_idx] = Some(metadata);

            append_padded(out, table_data, max_output_size)?;

            metadata
        }
        // glyf table (also process loca table)
        else if table.tag == GLYF {
            let loca_idx =
                loca_idx.expect("We already returned an error if glyf is present but loca isn't");

            // Generate transformed glyf and loca tables
            let raw_glyf_table_data = table.data_as_slice(woff_data)?;
            let glyf_and_loca_data = tranform_glyf_table(raw_glyf_table_data, max_output_size)?;

            // The origLength of the loca table declared in the table directory must exactly
            // match the size of the reconstructed loca table.
            // <https://www.w3.org/TR/WOFF2/#conform-mustRejectLoca>
            bail_with_msg_if!(
                tables[loca_idx].orig_length as usize != glyf_and_loca_data.loca_table.len(),
                "loca table origLength does not match reconstructed loca size"
            );

            // Store num_glyphs and x_mins
            num_glyphs = Some(glyf_and_loca_data.num_glyphs);
            x_mins = Some(glyf_and_loca_data.x_mins);

            // Write glyf table
            let glyf_dest_offset = out.len();
            append_padded(out, &glyf_and_loca_data.glyf_table, max_output_size)?;
            let glyf_metadata = TableMetadata {
                checksum: glyf_and_loca_data.glyf_checksum,
                dst_offset: glyf_dest_offset as u32,
                dst_length: glyf_and_loca_data.glyf_table.len() as u32,
            };
            table_metadata[table_idx] = Some(glyf_metadata);

            // Write loca table
            let loca_dest_offset = out.len();
            append_padded(out, &glyf_and_loca_data.loca_table, max_output_size)?;
            let loca_metdata = TableMetadata {
                checksum: glyf_and_loca_data.loca_checksum,
                dst_offset: loca_dest_offset as u32,
                dst_length: glyf_and_loca_data.loca_table.len() as u32,
            };
            table_metadata[loca_idx] = Some(loca_metdata);

            // Return glyf metadata
            glyf_metadata
        }
        // A "loca" table whose metadata has not already been computed. Only the font's
        // `loca_idx` is processed alongside "glyf" (and "glyf" sorts first), so this is a
        // second "loca" within the same font. Table tags must be unique within a font, so
        // reject.
        else if table.tag == LOCA {
            bail!()
        }
        // hmtx table
        else if table.tag == HMTX {
            // Tables are sorted so all the info we need has been gathered.
            // TODO: better error_handling
            let num_glyphs = num_glyphs.ok_or(WuffErr::GenericError)?;
            let num_hmetrics = num_hmetrics.ok_or(WuffErr::GenericError)?;
            let x_mins = x_mins.as_ref().ok_or(WuffErr::GenericError)?;

            // Generate reconstructed hmtx table
            let mut raw_hmtx_table_data = table.data_as_slice(woff_data)?;
            let hmtx_data = decode_hmtx_table(
                &mut raw_hmtx_table_data,
                num_glyphs,
                num_hmetrics,
                x_mins,
                max_output_size,
            )?;
            let hmtx_table = generate_hmtx_table(&hmtx_data, max_output_size)?;
            let checksum = compute_checksum(&hmtx_table);

            // Write table to output buffer
            let dest_offset = out.len();
            append_padded(out, &hmtx_table, max_output_size)?;
            // Note: like the reference implementation, we record the origLength declared in
            // the WOFF2 table directory (rather than the size of the reconstructed table)
            // in the output table directory entry. The two may legitimately differ.
            let hmtx_metadata = TableMetadata {
                checksum,
                dst_offset: dest_offset as u32,
                dst_length: table.orig_length,
            };
            table_metadata[table_idx] = Some(hmtx_metadata);

            hmtx_metadata
        } else {
            bail!()
        };

        // Update font checksum with the checksum for the table
        font_checksum = font_checksum.wrapping_add(metadata.checksum);

        // update the table entry with real values. We replaced 0's, so update  checksum.
        out_header.update_table_entry(font_idx, table.tag, metadata);
        font_checksum = font_checksum.wrapping_add(metadata.header_checksum_contribution());

        // The table (as recorded in the output table directory) must not extend past the end
        // of the data written (including padding) so far.
        bail_if!(metadata.dst_offset as u64 + metadata.dst_length as u64 > out.len() as u64);
    }

    // Update 'head' checkSumAdjustment. We already set it to 0 and summed font.
    //
    // The 'head' table is a special case in checksum calculations, as it includes a checksumAdjustment field
    // that is calculated and written after the table’s checksum is calculated and written into the table directory entry,
    // necessarily invalidating that checksum value.
    //
    // When generating font data, to calculate and write the 'head' table checksum and checksumAdjustment field, do the following:
    //
    //   1. Set the checksumAdjustment field to 0.
    //   2. Calculate the checksum for all tables including the 'head' table and enter the value
    //      for each table into the corresponding record in the table directory.
    //   3. Calculate the checksum for the entire font.
    //   4. Subtract that value from 0xB1B0AFBA.
    //   5. Store the result in the 'head' table checksumAdjustment field.
    //
    // <https://learn.microsoft.com/en-us/typography/opentype/spec/otff#calculating-checksums>
    let checksum_adjustment = 0xB1B0AFBA_u32.wrapping_sub(font_checksum);
    if let Some(head_table_idx) = font_entry.head_idx {
        let head_table_metadata = &table_metadata[head_table_idx as usize]
            .expect("Every table in the font should have metadata at this point");
        let mut writer = &mut out[head_table_metadata.dst_offset as usize + 8..];
        writer.put_u32(checksum_adjustment);
    }

    Ok(())
}

// Get numberOfHMetrics, https://www.microsoft.com/typography/otspec/hhea.htm
fn read_num_hmetrics(mut hhea_data: &[u8]) -> Result<u16, WuffErr> {
    bail_if!(hhea_data.remaining() < 34);
    hhea_data.advance(34); // Skip 34 to reach 'hhea' numberOfHMetrics
    Ok(hhea_data.try_get_u16()?)
}

struct HeaderData {
    data: Vec<u8>,
    checksum: u32,
    font_infos: Vec<WOFF2FontInfo>,
}

#[derive(Clone, Copy, Default)]
struct TableMetadata {
    checksum: u32,
    dst_offset: u32,
    dst_length: u32,
}

impl TableMetadata {
    pub fn is_already_computed(&self) -> bool {
        self.dst_offset != 0
    }

    pub fn header_checksum_contribution(&self) -> u32 {
        self.checksum
            .wrapping_add(self.dst_offset)
            .wrapping_add(self.dst_length)
    }
}

impl HeaderData {
    /// Update the table entry with real values.
    fn update_table_entry(&mut self, font_idx: usize, tag: Tag, metadata: TableMetadata) {
        // Write data
        let table_entry_offset = self.font_infos[font_idx]
            .table_entry_by_tag
            .binary_search_by_key(&tag, |(entry_tag, _)| *entry_tag)
            .map(|index| self.font_infos[font_idx].table_entry_by_tag[index].1)
            .expect("font table entry was indexed while its header was generated");

        let mut out = &mut self.data[(table_entry_offset + 4)..(table_entry_offset + 16)];
        out.put_u32(metadata.checksum);
        out.put_u32(metadata.dst_offset);
        out.put_u32(metadata.dst_length);

        // Update checksum
        let mut checksum = self.font_infos[font_idx].header_checksum;
        checksum = checksum.wrapping_add(metadata.checksum);
        checksum = checksum.wrapping_add(metadata.dst_offset);
        checksum = checksum.wrapping_add(metadata.dst_length);
        self.font_infos[font_idx].header_checksum = checksum;
    }
}

fn generate_header(
    header: &WoffHeader,
    tables: &TableDirectory,
    collection_directory: &CollectionDirectory,
    max_output_size: usize,
) -> Result<HeaderData, WuffErr> {
    let num_fonts = collection_directory.fonts.len();
    let size_of_header = collection_directory
        .table_directories_required_size_limited(header.is_collection(), max_output_size)?;
    let mut output = vec_with_capacity(size_of_header, max_output_size)?;
    let mut font_infos = vec_with_capacity(num_fonts, max_output_size)?;
    for font in &collection_directory.fonts {
        let table_entry_by_tag = vec_with_capacity(font.table_indices.len(), max_output_size)?;
        font_infos.push(WOFF2FontInfo {
            table_entry_by_tag,
            ..WOFF2FontInfo::default()
        });
    }

    let mut checksum: u32 = 0;

    // If TTC: write TTC header
    if header.is_collection() {
        // TTC header
        output.put_u32(u32::from_be_bytes(header.flavor.to_be_bytes())); // TAG TTCTag
        output.put_u32(collection_directory.version); // FIXED Version
        output.put_u32(num_fonts as u32); // ULONG numFonts

        // let mut offset_table_idx: usize = output.len(); // keep start of offset table for later

        // Write tableDirectoryOffsets
        let first_table_directory_offset = match collection_directory.version {
            0x00010000 => 12 + (4 * num_fonts as u32),
            0x00020000 => 12 + 12 + (4 * num_fonts as u32),
            _ => unreachable!("Only 1.0 and 2.0 are supported versions"),
        };
        let mut table_directory_offset = first_table_directory_offset;
        for font in collection_directory.fonts.iter() {
            output.put_u32(table_directory_offset);
            table_directory_offset += font.table_directory_size() as u32;
        }

        // space for DSIG fields for header v2
        if collection_directory.version == 0x00020000 {
            output.put_u32(0); // ULONG ulDsigTag
            output.put_u32(0); // ULONG ulDsigLength
            output.put_u32(0); // ULONG ulDsigOffset
        }

        checksum = checksum.wrapping_add(compute_checksum(&output));
    }

    // Write table directory(s)
    // If file is a TTC: one per font. Else for a single font: one in total.
    for (font, info) in collection_directory.fonts.iter().zip(font_infos.iter_mut()) {
        // write the actual offset table so our header doesn't lie
        // font.dst_offset = offset as u32;
        let start_offset = output.len();
        write_table_directory_header(&mut output, font.flavor, font.table_indices.len() as u16);

        for &table_index in &font.table_indices {
            let tag = tables[table_index as usize].tag;
            info.table_entry_by_tag.push((tag, output.len()));
            write_empty_offset_table_entry(&mut output, tag);
        }

        info.header_checksum = compute_checksum(&output[start_offset..]);
        checksum = checksum.wrapping_add(info.header_checksum);
    }

    bail_if!(output.len() != size_of_header);
    Ok(HeaderData {
        data: output,
        font_infos,
        checksum,
    })
}

// Writes a single Offset Table entry
fn write_empty_offset_table_entry(output: &mut impl BufMut, tag: Tag) {
    output.put_u32(u32::from_be_bytes(tag.to_be_bytes()));
    output.put_u32(0);
    output.put_u32(0);
    output.put_u32(0);
}

#[cfg(all(test, feature = "brotli"))]
mod tests {
    use super::decompress_woff2_with_custom_brotli_limited;
    use alloc::{boxed::Box, vec::Vec};
    use core::error::Error;

    fn decode_limited(input: &[u8], limit: usize) -> Result<Vec<u8>, crate::WuffErr> {
        decompress_woff2_with_custom_brotli_limited(
            input,
            limit,
            &mut crate::brotli::decompress_brotli,
        )
    }

    #[test]
    fn valid_font_decodes_at_exact_reconstructed_size_limit() {
        let input = include_bytes!("../tests/fixtures/valid-001.woff2");
        let decoded = crate::decompress_woff2(input).expect("upstream valid WOFF2 fixture");

        assert_eq!(decode_limited(input, decoded.len()).unwrap(), decoded);
        assert!(decode_limited(input, decoded.len() - 1).is_err());
    }

    #[test]
    fn output_limit_rejects_before_calling_brotli() {
        let input = include_bytes!("../tests/fixtures/valid-001.woff2");
        let mut called = false;
        let result =
            decompress_woff2_with_custom_brotli_limited(input, 1, &mut |_compressed: &[u8],
                                                                        _expected: usize|
             -> Result<
                _,
                Box<dyn Error>,
            > {
                called = true;
                Ok(Vec::new())
            });

        assert!(result.is_err());
        assert!(
            !called,
            "oversized table stream reached the Brotli callback"
        );
    }

    #[test]
    fn callback_output_must_match_bounded_table_stream_length() {
        let input = include_bytes!("../tests/fixtures/valid-001.woff2");
        let result = decompress_woff2_with_custom_brotli_limited(
            input,
            crate::MAX_WOFF2_OUTPUT_SIZE,
            &mut |_compressed: &[u8], expected: usize| -> Result<_, Box<dyn Error>> {
                Ok(alloc::vec![0; expected.saturating_add(1)])
            },
        );

        assert!(result.is_err());
    }

    #[test]
    fn advisory_header_and_transformed_lengths_do_not_define_output_bound() {
        for input in [
            &include_bytes!("../tests/fixtures/header-totalsfntsize-001.woff2")[..],
            &include_bytes!("../tests/fixtures/header-totalsfntsize-002.woff2")[..],
            &include_bytes!("../tests/fixtures/tabledata-glyf-origlength-003.woff2")[..],
        ] {
            let decoded = crate::decompress_woff2(input)
                .expect("WPT fixture requires accepting advisory length metadata");
            assert_eq!(decode_limited(input, decoded.len()).unwrap(), decoded);
        }
    }

    #[test]
    fn overlapping_private_block_is_rejected() {
        let input = include_bytes!("../tests/fixtures/blocks-overlap-002.woff2");
        assert!(decode_limited(input, crate::MAX_WOFF2_OUTPUT_SIZE).is_err());
    }
}
