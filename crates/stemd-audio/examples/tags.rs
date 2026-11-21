//! Print the tags a file declares, for checking what a container really has.

fn main() {
    for arg in std::env::args().skip(1) {
        let path = std::path::Path::new(&arg);
        let tags = stemd_audio::tags(path);
        println!("{arg}");
        println!("  title : {:?}", tags.title);
        println!("  artist: {:?}", tags.artist);
        println!("  album : {:?}", tags.album);
    }
}
