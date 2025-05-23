use clap::Parser;
use std::num::NonZeroUsize;

use core::ffi;
use minimap2_sys::MmIdx;
// Or now
// use minimap2::ffi as mm_ffi;
//use minimap2_temp as minimap2;
use num_format::{Locale, ToFormattedString};
use std::io::Read;
use std::sync::Arc;
use std::{fs::File, io};

use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, filter::LevelFilter, fmt, prelude::*};

use noodles_bam as bam;
use noodles_bgzf as bgzf;
use noodles_sam::header::record::value as header_val;
use noodles_sam::header::record::value::Map as HeaderMap;

mod alignment_parser;
mod bootstrap;
mod bulk;
mod em;
mod prog_opts;
mod single_cell;
mod util;

use crate::prog_opts::{Args, FilterGroup, SequencingTech};
use crate::util::digest_utils;
use crate::util::normalize_probability::normalize_read_probs;
use crate::util::oarfish_types::{AlignmentFilters, TranscriptInfo};
use crate::util::{
    binomial_probability::binomial_continuous_prob, kde_utils, logistic_probability::logistic_prob,
};

type HeaderReaderAlignerDigest = (
    noodles_sam::header::Header,
    Option<bam::io::Reader<bgzf::MultithreadedReader<File>>>,
    Option<minimap2::Aligner<minimap2::Built>>,
    seqcol_rs::DigestResult,
);

fn is_fasta(fname: &std::path::Path) -> anyhow::Result<bool> {
    match std::fs::OpenOptions::new().read(true).open(fname) {
        Ok(mut file) => {
            let mut first_char = vec![0_u8];
            file.read_exact(&mut first_char)?;
            drop(file);
            Ok(first_char[0] == b'>' || first_char[0] == b'@')
        }
        _ => Ok(false),
    }
}

fn get_aligner_from_args(args: &mut Args) -> anyhow::Result<HeaderReaderAlignerDigest> {
    info!("oarfish is operating in read-based mode");

    let ref_file = args
        .reference
        .clone()
        .expect("must provide reference sequence");

    let ref_file_clone = ref_file.clone();
    // The `ref_file` input argument is either a FASTA file with reference
    // sequences, in which case we will compute the proper digest in a separate
    // thread, OR an existing minimap2 index, in which case we won't attempt
    // to treat it as a FASTA file and we will later get the digest from
    // the index.
    let digest_handle = if is_fasta(&ref_file).unwrap_or(false) {
        Some(std::thread::spawn(|| {
            info!("generating reference digest");
            let mut seqcol_obj = seqcol_rs::SeqCol::try_from_fasta_file(ref_file_clone).unwrap();
            let digest = seqcol_obj.digest(seqcol_rs::DigestConfig {
                level: seqcol_rs::DigestLevel::Level1,
                additional_attr: vec![seqcol_rs::KnownAttr::SortedNameLengthPairs],
            });
            info!("done");
            digest
        }))
    } else {
        // if the input was not a FASTA file, then don't attempt to
        // write out another index, because we are reading one in!
        if args.index_out.is_some() {
            warn!(
                "The `--index-out` flag is set, but the input already appears to be an index; skipping writing of output index"
            );
            args.index_out = None;
        }
        info!("Reading existing minimap2 index that was not created by oarfish.");
        None
    };

    let thread_sub = if digest_handle.is_some() { 1 } else { 0 };
    // set the number of indexing threads
    let idx_threads = &args.threads.saturating_sub(thread_sub).max(1);

    // if the user requested to write the output index to disk, prepare for that
    let idx_out_as_str = args.index_out.clone().map_or(String::new(), |x| {
        x.to_str()
            .expect("could not convert PathBuf to &str")
            .to_owned()
    });
    let idx_output = args.index_out.as_ref().map(|_| idx_out_as_str.as_str());

    // create the aligner
    let mut aligner = match args.seq_tech {
        Some(SequencingTech::OntCDNA) | Some(SequencingTech::OntDRNA) => {
            minimap2::Aligner::builder()
                .map_ont()
                .with_index_threads(*idx_threads)
                .with_cigar()
                .with_index(
                    args.reference
                        .clone()
                        .expect("must provide reference sequence"),
                    idx_output,
                )
                .expect("could not construct minimap2 index")
        }
        Some(SequencingTech::PacBio) => minimap2::Aligner::builder()
            .map_pb()
            .with_index_threads(*idx_threads)
            .with_cigar()
            .with_index(
                args.reference
                    .clone()
                    .expect("must provide reference sequence"),
                idx_output,
            )
            .expect("could not construct minimap2 index"),
        Some(SequencingTech::PacBioHifi) => minimap2::Aligner::builder()
            .map_hifi()
            .with_index_threads(*idx_threads)
            .with_cigar()
            .with_index(
                args.reference
                    .clone()
                    .expect("must provide reference sequence"),
                idx_output,
            )
            .expect("could not construct minimap2 index"),
        None => {
            anyhow::bail!("sequencing tech must be provided in read mode, but it was not!");
        }
    };

    info!("created aligner index opts : {:?}", aligner.idxopt);
    // get up to the best_n hits for each read
    // default value is 100.
    aligner.mapopt.best_n = args.best_n as i32;
    // set the seed to be the same as what command-line
    // minimap2 uses.
    aligner.mapopt.seed = 11;

    let n_seq = aligner.n_seq();

    info!(
        "index contains {} sequences",
        n_seq.to_formatted_string(&Locale::en)
    );

    let mut header = noodles_sam::header::Header::builder();

    #[derive(Debug, PartialEq, Eq)]
    pub struct SeqMetaData {
        pub name: String,
        pub length: u32,
        pub is_alt: bool,
    }

    // TODO: better creation of the header
    {
        for i in 0..n_seq {
            let seq = aligner.get_seq(i as usize).unwrap_or_else(|| {
                panic!(
                    "{} was not a valid reference sequence index. (n_seq = {})",
                    i, n_seq
                )
            });
            let c_str = unsafe { ffi::CStr::from_ptr(seq.name) };
            let rust_str = c_str.to_str().unwrap().to_string();
            header = header.add_reference_sequence(
                rust_str,
                HeaderMap::<header_val::map::ReferenceSequence>::new(NonZeroUsize::try_from(
                    seq.len as usize,
                )?),
            );
        }
    }

    header = header.add_program(
        "minimap2-rs",
        HeaderMap::<header_val::map::Program>::default(),
    );

    let header = header.build();

    let digest = match digest_handle {
        // we are building the digest from an input fasta file
        Some(digest_handle_inner) => {
            let digest_res = digest_handle_inner.join().expect("valid digest");
            let digest = digest_res?;
            // if we created an index, append the digest
            if let Some(idx_file) = idx_output {
                digest_utils::append_digest_to_mm2_index(idx_file, &digest)?;
            }
            digest
        }
        _ => {
            match digest_utils::read_digest_from_mm2_index(
                ref_file.to_str().expect("could not convert to string"),
            ) {
                // we read a pre-computed digest from an oarfish-constructed
                // minimap2 index
                Ok(d) => d,
                _ => {
                    // We have been given a minimap2 index, but without the oarfish
                    // footer. Now, we can build the digest we want from the index
                    // itself.
                    warn!(
                        "computing sequence signatures from a minimap2 index that was not built with oarfish."
                    );
                    warn!(
                        "if you are quantifying multiple samples, it will save time to let oarfish build a minimap2 index from the transcriptome reference, so that the reference signature can be reused."
                    );
                    let mmi: Arc<MmIdx> = Arc::clone(aligner.idx.as_ref().unwrap());
                    digest_utils::digest_from_index(&mmi)?
                }
            }
        }
    };

    Ok((header, None, Some(aligner), digest))
}

fn get_filter_opts(args: &Args) -> anyhow::Result<AlignmentFilters> {
    // set all of the filter options that the user
    // wants to apply.
    match args.filter_group {
        Some(FilterGroup::NoFilters) => {
            info!("disabling alignment filters.");
            // override individual parameters if the user passed them in explicitly
            let fpc = args
                .five_prime_clip
                .provided_or_u32("overriding 5' clip with user-provided value", u32::MAX);
            let tpc = args
                .three_prime_clip
                .provided_or_i64("overriding 3' clip with user-provided value", i64::MAX);
            let st = args
                .score_threshold
                .provided_or_f32("overriding score threshold with user-provided value", 0_f32);
            let maf = args.min_aligned_fraction.provided_or_f32(
                "overriding min aligned fraction with user-provided value",
                0_f32,
            );
            let mal = args.min_aligned_len.provided_or_u32(
                "overriding min aligned length with user-provided value",
                1_u32,
            );

            Ok(AlignmentFilters::builder()
                .five_prime_clip(fpc)
                .three_prime_clip(tpc)
                .score_threshold(st)
                .min_aligned_fraction(maf)
                .min_aligned_len(mal)
                .which_strand(args.strand_filter)
                .model_coverage(args.model_coverage)
                .logistic_growth_rate(args.growth_rate)
                .write_assignment_probs(args.write_assignment_probs.is_some())
                .write_assignment_probs_type(args.write_assignment_probs.clone())
                .build())
        }
        Some(FilterGroup::NanocountFilters) => {
            info!("setting filters to nanocount defaults.");
            // override individual parameters if the user passed them in explicitly
            let fpc = args
                .five_prime_clip
                .provided_or_u32("overriding 5' clip with user-provided value", u32::MAX);
            let tpc = args
                .three_prime_clip
                .provided_or_i64("overriding 3' clip with user-provided value", 50_i64);
            let st = args.score_threshold.provided_or_f32(
                "overriding score threshold with user-provided value",
                0.95_f32,
            );
            let maf = args.min_aligned_fraction.provided_or_f32(
                "overriding min aligned fraction with user-provided value",
                0.5_f32,
            );
            let mal = args.min_aligned_len.provided_or_u32(
                "overriding min aligned length with user-provided value",
                50_u32,
            );

            Ok(AlignmentFilters::builder()
                .five_prime_clip(fpc)
                .three_prime_clip(tpc)
                .score_threshold(st)
                .min_aligned_fraction(maf)
                .min_aligned_len(mal)
                .which_strand(bio_types::strand::Strand::Forward)
                .model_coverage(args.model_coverage)
                .logistic_growth_rate(args.growth_rate)
                .write_assignment_probs(args.write_assignment_probs.is_some())
                .write_assignment_probs_type(args.write_assignment_probs.clone())
                .build())
        }
        None => {
            info!("setting user-provided filter parameters.");
            Ok(AlignmentFilters::builder()
                .five_prime_clip(args.five_prime_clip.try_as_u32()?)
                .three_prime_clip(args.three_prime_clip.try_as_i64()?)
                .score_threshold(args.score_threshold.try_as_f32()?)
                .min_aligned_fraction(args.min_aligned_fraction.try_as_f32()?)
                .min_aligned_len(args.min_aligned_len.try_as_u32()?)
                .which_strand(args.strand_filter)
                .model_coverage(args.model_coverage)
                .logistic_growth_rate(args.growth_rate)
                .write_assignment_probs(args.write_assignment_probs.is_some())
                .write_assignment_probs_type(args.write_assignment_probs.clone())
                .build())
        }
    }
}

fn main() -> anyhow::Result<()> {
    let env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();
    let (filtered_layer, reload_handle) = tracing_subscriber::reload::Layer::new(env_filter);

    // set up the logging.  Here we will take the
    // logging level from the environment variable if
    // it is set.  Otherwise, we'll set the default
    tracing_subscriber::registry()
        // log level to INFO.
        .with(fmt::layer().with_writer(io::stderr))
        .with(filtered_layer)
        .init();

    let mut args = Args::parse();

    // change the logging filter if the user specified quiet or
    // verbose.
    if args.quiet {
        reload_handle.modify(|filter| *filter = EnvFilter::new("WARN"))?;
    }
    if args.verbose {
        reload_handle.modify(|filter| *filter = EnvFilter::new("TRACE"))?;
    }

    let filter_opts = get_filter_opts(&args)?;

    let (header, reader, aligner, digest) = if args.alignments.is_none() {
        get_aligner_from_args(&mut args)?
    } else {
        let alignments = args.alignments.clone().unwrap();
        let afile = File::open(&alignments)?;

        let decomp_threads = if args.single_cell {
            // we will overlap quantification with parsing, so don't try to use too many
            // parser threads, and adjust the worker threads accordingly.

            // is there a better heuristic than this?
            // <= 6 threads, use only 1 for decompression
            // 6-8 threads, use 2 for decompression
            // > 8 threads, use 3 for decompression
            match args.threads {
                1..=6 => 1,
                7 | 8 => 2,
                _ => 3,
            }
        } else {
            // try to use all but 1 thread, and assume we have at least 2.
            1.max(args.threads.saturating_sub(1))
        };

        let worker_count = NonZeroUsize::new(decomp_threads).expect("decompression threads >= 1");
        if args.single_cell {
            args.threads = 1.max(args.threads.saturating_sub(decomp_threads));
        }

        let decoder = bgzf::MultithreadedReader::with_worker_count(worker_count, afile);
        let mut reader = bam::io::Reader::from(decoder);
        // parse the header, and ensure that the reads were mapped with minimap2 (as far as we
        // can tell).
        let header = alignment_parser::read_and_verify_header(&mut reader, &alignments)?;
        let seqcol_digest = digest_utils::digest_from_header(&header)?;
        (header, Some(reader), None, seqcol_digest)
    };

    let num_ref_seqs = header.reference_sequences().len();

    // where we'll write down the per-transcript information we need
    // to track.
    let mut txps: Vec<TranscriptInfo> = Vec::with_capacity(num_ref_seqs);
    let mut txps_name: Vec<String> = Vec::with_capacity(num_ref_seqs);

    // loop over the transcripts in the header and fill in the relevant
    // information here.
    if args.model_coverage {
        for (rseq, rmap) in header.reference_sequences().iter() {
            txps.push(TranscriptInfo::with_len_and_bin_width(
                rmap.length(),
                args.bin_width,
            ));
            txps_name.push(rseq.to_string());
        }
    } else {
        for (rseq, rmap) in header.reference_sequences().iter() {
            txps.push(TranscriptInfo::with_len(rmap.length()));
            txps_name.push(rseq.to_string());
        }
    }
    info!(
        "parsed reference information for {} transcripts.",
        txps.len().to_formatted_string(&Locale::en)
    );

    if args.single_cell {
        // TODO: do this better (quiet the EM during single-cell quant)
        reload_handle.modify(|filter| {
            *filter = if args.quiet {
                EnvFilter::new("WARN")
                    .add_directive("oarfish=warn".parse().unwrap())
                    .add_directive("oarfish::single_cell=warn".parse().unwrap())
            } else if args.verbose {
                EnvFilter::new("TRACE")
                    .add_directive("oarfish=info".parse().unwrap())
                    .add_directive("oarfish::single_cell=trace".parse().unwrap())
            } else {
                // be quiet about normal things in single-cell mode
                // e.g. EM iterations, and only print out info for
                // oarfish::single_cell events.
                EnvFilter::new("INFO")
                    .add_directive("oarfish=warn".parse().unwrap())
                    .add_directive("oarfish::single_cell=info".parse().unwrap())
            }
        })?;

        single_cell::quantify_single_cell_from_collated_bam(
            &header,
            &filter_opts,
            &mut reader.unwrap(),
            &mut txps,
            &args,
            digest,
        )?;
    } else if args.alignments.is_some() {
        bulk::quantify_bulk_alignments_from_bam(
            &header,
            filter_opts,
            &mut reader.unwrap(),
            &mut txps,
            &txps_name,
            &args,
            digest,
        )?;
    } else {
        bulk::quantify_bulk_alignments_raw_reads(
            &header,
            aligner.expect("need valid alinger to align reads"),
            filter_opts,
            &args.reads.clone().expect("expected read file(s)"),
            &mut txps,
            &txps_name,
            &args,
            digest,
        )?;
    }

    info!("oarfish completed successfully.");
    Ok(())
}
