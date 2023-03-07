#![feature(int_roundings)]

use std::{
	collections::HashMap,
	ffi::OsStr,
	fs::File,
	io::Read,
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use compress::zlib;
use fuser::{FileAttr, FileType, MountOption, FUSE_ROOT_ID};
use log::info;
use roead::sarc;

// How long fuse will cache a result
const TTL: Duration = Duration::from_millis(100);
const BLOCK_SIZE: u32 = 4096;

fn main() {
	env_logger::init();

	let args = clap::Command::new("test prog")
		.author("0xadk")
		.version("0.0.1")
		.about("A fuse filesystem for zlib compressed SARC archives")
		.arg(clap::Arg::new("archive").required(true).index(1))
		.arg(clap::Arg::new("mount_point").required(true).index(2))
		.get_matches();

	let archive_path = args.get_one::<String>("archive").unwrap().to_string();
	let mount_point = args.get_one::<String>("mount_point").unwrap();

	let fs = FuseFileSystem {
		archive_path,
		..FuseFileSystem::default()
	};

	let mount_session = fuser::spawn_mount2(fs, mount_point, &vec![
		MountOption::RO,
		MountOption::FSName("rhmmfs".to_string()),
	])
	.unwrap();

	// wait until ctrl-c is pressed to unmount the
	// fuse fs and resume the main thread
	ctrlc::set_handler({
		let main_thread = std::thread::current();

		move || {
			main_thread.unpark();
		}
	})
	.unwrap();

	std::thread::park();
	mount_session.join();
}

type INode = u64;

#[derive(Default)]
struct FuseFileSystem {
	archive_path: String,

	next_inode: u64,
	attr_map: HashMap<INode, FileAttr>,
	dir_children: HashMap<INode, HashMap<String, INode>>,

	archive: Option<sarc::SarcWriter>,
	full_file_paths: HashMap<INode, String>,
}

impl FuseFileSystem {
	fn get_new_inode(&mut self) -> u64 {
		let inode = self.next_inode;
		self.next_inode += 1;
		inode
	}

	fn add_new_dir_attr(&mut self, inode: INode, time: SystemTime, uid: u32, gid: u32) {
		self.dir_children.insert(inode, HashMap::new());

		self.attr_map.insert(inode, FileAttr {
			ino: inode,
			size: BLOCK_SIZE as u64,
			blocks: (BLOCK_SIZE / 512) as u64,
			atime: time,
			mtime: time,
			ctime: time,
			crtime: time,
			kind: FileType::Directory,
			perm: 0o755,
			nlink: 1,
			uid,
			gid,
			rdev: 0,
			blksize: BLOCK_SIZE,
			flags: 0,
		});
	}

	fn add_new_file_attr(&mut self, inode: INode, size: u64, time: SystemTime, uid: u32, gid: u32) {
		self.attr_map.insert(inode, FileAttr {
			ino: inode,
			size,
			// unintuitively the number of 512 byte blocks, not BLOCK_SIZE blocks
			blocks: size.div_ceil(512),
			atime: time,
			mtime: time,
			ctime: time,
			crtime: time,
			kind: FileType::RegularFile,
			perm: 0o644,
			nlink: 1,
			uid,
			gid,
			rdev: 0,
			blksize: BLOCK_SIZE,
			flags: 0,
		});
	}
}

impl fuser::Filesystem for FuseFileSystem {
	fn init(
		&mut self,
		_req: &fuser::Request<'_>,
		_config: &mut fuser::KernelConfig,
	) -> Result<(), libc::c_int> {
		let mut zlib_archive = File::open(&self.archive_path).unwrap();

		let uncompressed_archive_size = {
			// the first 4 bytes should be the size of the uncompressed SARC archive
			let mut bytes = [0_u8; 4];
			zlib_archive.read_exact(&mut bytes).unwrap();

			u32::from_be_bytes(bytes)
		};

		let mut uncompressed_data = Vec::new();
		zlib::Decoder::new(zlib_archive)
			.read_to_end(&mut uncompressed_data)
			.unwrap();

		assert!(
			uncompressed_archive_size as usize == uncompressed_data.len(),
			"expected size: {}, actual size: {}",
			uncompressed_archive_size,
			uncompressed_data.len()
		);

		// let mut output_file = File::options()
		// 	.create(true)
		// 	.write(true)
		// 	.open("./output.bin")
		// 	.unwrap();
		//
		// output_file.write(&uncompressed_data).unwrap();

		if !uncompressed_data.starts_with(b"SARC") {
			eprintln!("[Error] uncompressed data isn't a SARC archive");
			return Err(libc::ENOSYS);
		}

		info!(
			"uncompressed SARC archive size: {}",
			uncompressed_archive_size
		);

		let sarc_archive = sarc::Sarc::new(&uncompressed_data).unwrap();

		// sarc_archive.get_data();

		let archive_files: Vec<_> = sarc_archive.files().collect();
		info!("archive contains {} files", archive_files.len());

		// FIXME: the uid and gid shouldn't be hard coded
		let (uid, gid) = (1000, 1000);

		// setup root dir
		self.add_new_dir_attr(FUSE_ROOT_ID, SystemTime::now(), uid, gid);
		self.next_inode = FUSE_ROOT_ID + 1;

		let mut unresolved_paths: Vec<(INode, Vec<(&str, &str, usize)>)> = vec![(
			FUSE_ROOT_ID,
			archive_files
				.iter()
				.filter_map(|f| f.name.map(|n| (n, n, f.data.len())))
				.collect(),
		)];

		while let Some((dir_inode, data)) = unresolved_paths.pop() {
			let mut new_dirs = HashMap::<&str, Vec<(&str, &str, usize)>>::new();

			for (path, full_path, size) in data {
				match path.split_once('/') {
					Some((dir_name, unresolved_path)) => {
						if !new_dirs.contains_key(dir_name) {
							new_dirs.insert(dir_name.into(), Vec::new());
						}

						new_dirs.get_mut(dir_name).unwrap().push((
							unresolved_path.into(),
							full_path,
							size,
						));
					},

					None => {
						let file_inode = self.get_new_inode();

						self.full_file_paths
							.insert(file_inode, full_path.to_string());

						self.add_new_file_attr(file_inode, size as u64, UNIX_EPOCH, uid, gid);

						self.dir_children
							.get_mut(&dir_inode)
							.unwrap()
							.insert(path.to_string(), file_inode);
					},
				}
			}

			for (new_dir, paths) in new_dirs.into_iter() {
				let new_dir_inode = self.get_new_inode();
				self.dir_children
					.get_mut(&dir_inode)
					.unwrap()
					.insert(new_dir.to_string(), new_dir_inode);

				self.add_new_dir_attr(new_dir_inode, UNIX_EPOCH, uid, gid);
				unresolved_paths.push((new_dir_inode, paths));
			}
		}

		// I doubt this is efficient
		self.archive = Some(sarc::SarcWriter::from_sarc(&sarc_archive));

		Ok(())
	}

	fn lookup(
		&mut self,
		_req: &fuser::Request<'_>,
		dir_inode: INode,
		name: &OsStr,
		reply: fuser::ReplyEntry,
	) {
		let children = match self.dir_children.get(&dir_inode) {
			None => {
				reply.error(libc::ENOENT);
				return;
			},

			Some(c) => c,
		};

		let file_inode = match children.get(&name.to_str().unwrap().to_string()) {
			None => {
				reply.error(libc::ENOENT);
				return;
			},

			Some(inode) => inode,
		};

		match self.attr_map.get(&file_inode) {
			None => reply.error(libc::ENOENT),
			Some(attr) => reply.entry(&TTL, attr, 0),
		};
	}

	fn getattr(&mut self, _req: &fuser::Request<'_>, inode: u64, reply: fuser::ReplyAttr) {
		match self.attr_map.get(&inode) {
			None => reply.error(libc::ENOENT),
			Some(attr) => reply.attr(&TTL, attr),
		};
	}

	fn read(
		&mut self,
		_req: &fuser::Request<'_>,
		inode: u64,
		_fh: u64,
		offset: i64,
		size: u32,
		flags: i32,
		lock_owner: Option<u64>,
		reply: fuser::ReplyData,
	) {
		let data = self
			.full_file_paths
			.get(&inode)
			.and_then(|path| self.archive.as_mut().unwrap().get_file(path));

		match data {
			None => reply.error(libc::ENOENT),
			Some(data) => {
				let start = offset as usize;
				let end = offset as usize + size as usize;
				reply.data(&data[start..end.min(data.len())])
			},
		};
	}

	fn readdir(
		&mut self,
		_req: &fuser::Request<'_>,
		dir_inode: u64,
		_fh: u64,
		offset: i64,
		mut reply: fuser::ReplyDirectory,
	) {
		let dir_children = match self.dir_children.get(&dir_inode) {
			None => {
				reply.error(libc::ENOENT);
				return;
			},

			Some(children) => children,
		};

		for (i, (name, inode)) in dir_children.iter().enumerate().skip(offset as usize) {
			let attr = self.attr_map.get(&inode).unwrap();
			let reply_buffer_full = reply.add(*inode, (i + 1) as i64, attr.kind, name);

			if reply_buffer_full {
				break;
			}
		}

		reply.ok();
	}
}
