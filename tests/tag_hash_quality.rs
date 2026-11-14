//! The tag hash is the series identity (collisions are detected, not avoided),
//! so it must not collide on tag sets shaped like real ones. Generated metric
//! structs derive `Hash`; `tags_hash()` feeds them to `tag_hasher()`, which is
//! reproduced here.
use std::hash::{Hash, Hasher};

use chalk_metrics::__private::tag_hasher;
use chalk_metrics::{define_metrics, define_tags};

define_tags! {
    Endpoint => "endpoint";
    Region => "region";
    Tenant => "tenant";
    Id => "id";
    Path => "path";
    Dotted => "dotted";
    Status => "status" {
        Success => "success",
        Failure => "failure",
    }
    Method => "method" {
        Get => "get",
        Post => "post",
    }
}

define_metrics! {
    group(tags = [Endpoint, Status]) {
        count StringAndEnum => "string_and_enum", "One string and one enum tag";
    }
    group(tags = [Endpoint, Region, Tenant, Status, Method]) {
        count FiveTags => "five_tags", "Three string and two enum tags";
    }
    group(tags = [Id, Path, Dotted]) {
        count IdsPathsNames => "ids_paths_names", "Ids, paths and dotted names";
    }
}

fn hash_of(value: &impl Hash) -> u64 {
    let mut hasher = tag_hasher();
    value.hash(&mut hasher);
    hasher.finish()
}

/// Bijective 64-bit mixer, so distinct `i` give distinct, random-looking ids.
fn splitmix(i: u64) -> u64 {
    let mut z = i.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn status(bit: u64) -> Status {
    if bit == 0 {
        Status::Success
    } else {
        Status::Failure
    }
}

fn method(bit: u64) -> Method {
    if bit == 0 { Method::Get } else { Method::Post }
}

/// Hashes `total` distinct tag sets (40% string + enum, 30% each of the others)
/// and asserts that no two hashes are equal.
fn assert_no_collisions(total: u64) {
    const REGIONS: [&str; 8] = [
        "us-east-1",
        "us-west-2",
        "eu-west-1",
        "eu-central-1",
        "ap-south-1",
        "ap-northeast-1",
        "sa-east-1",
        "ca-central-1",
    ];
    let (a, b) = (total * 2 / 5, total * 3 / 10);
    let c = total - a - b;
    let mut hashes = Vec::with_capacity(total as usize);

    for i in 0..a {
        hashes.push(hash_of(&StringAndEnum {
            endpoint: Endpoint::from(format!("/api/v1/users/{}/orders", i / 2)),
            status: status(i % 2),
        }));
    }
    // Mixed radix: tenant (1000) x region (8) x status (2) x method (2), then endpoint.
    for i in 0..b {
        hashes.push(hash_of(&FiveTags {
            endpoint: Endpoint::from(format!("/v2/items/{}", i / 32_000)),
            region: Region::from(REGIONS[(i / 1000 % 8) as usize]),
            tenant: Tenant::from(format!("tenant-{:04}", i % 1000)),
            status: status(i / 8000 % 2),
            method: method(i / 16_000 % 2),
        }));
    }
    for i in 0..c {
        let id = splitmix(i);
        hashes.push(hash_of(&IdsPathsNames {
            id: Id::from(format!("{id:016x}")),
            path: Path::from(format!("/v1/{}/{}", id % 977, i)),
            dotted: Dotted::from(format!("svc.team{}.module{}.handler", i % 31, i % 7)),
        }));
    }

    assert_eq!(hashes.len() as u64, total);
    hashes.sort_unstable();
    let duplicates = hashes.windows(2).filter(|w| w[0] == w[1]).count();
    assert_eq!(duplicates, 0, "{duplicates} colliding hashes among {total}");
}

#[test]
fn tag_hash_is_deterministic() {
    let tags = || StringAndEnum {
        endpoint: Endpoint::from("/api"),
        status: Status::Success,
    };
    assert_eq!(hash_of(&tags()), hash_of(&tags()));
}

#[test]
fn five_million_tag_sets_do_not_collide() {
    assert_no_collisions(5_000_000);
}

#[test]
#[ignore = "50 million tag sets: slow and memory hungry"]
fn fifty_million_tag_sets_do_not_collide() {
    assert_no_collisions(50_000_000);
}
