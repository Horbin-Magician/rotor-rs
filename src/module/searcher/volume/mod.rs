mod file_map;
mod file_index;

use std::{
    env, error::Error, ffi::{c_void, CString}, fs, io::{self, Write}, sync::mpsc
};

use windows_sys::Win32::{
    Foundation::{self, INVALID_HANDLE_VALUE}, Storage::FileSystem, System::{Ioctl, IO}
};

#[allow(unused_imports)]
use std::time::SystemTime;
#[allow(unused_imports)]
use crate::util::log_util::{log_error, log_info};
use file_map::{FileMap, File, make_filter};
use file_index::FileIndex;


fn get_file_path(volume_handle: isize, file_id: u64) -> String {
    let file_id_desc = FileSystem::FILE_ID_DESCRIPTOR {
        Type: FileSystem::FileIdType,
        dwSize: size_of::<FileSystem::FILE_ID_DESCRIPTOR>() as u32,
        Anonymous: FileSystem::FILE_ID_DESCRIPTOR_0 {
            FileId: file_id as i64,
        },
    };

    unsafe {
        let file_handle = FileSystem::OpenFileById(
        volume_handle,
        &file_id_desc,
        FileSystem::FILE_GENERIC_READ,
        FileSystem::FILE_SHARE_READ | FileSystem::FILE_SHARE_WRITE | FileSystem::FILE_SHARE_DELETE,
        std::ptr::null_mut(),
        0,
        );

        if file_handle == INVALID_HANDLE_VALUE { return String::from(""); }

        let info_buffer_size =
        size_of::<FileSystem::FILE_NAME_INFO>() + (Foundation::MAX_PATH as usize) * size_of::<u16>();
        let mut info_buffer = vec![0u8; info_buffer_size];
        let info_result = FileSystem::GetFileInformationByHandleEx(
        file_handle,
        FileSystem::FileNameInfo,
        &mut *info_buffer as *mut _ as *mut c_void,
        info_buffer_size as u32,
        );

        Foundation::CloseHandle(file_handle);

        if info_result != 0 {
            let (_, body, _) = info_buffer.align_to::<FileSystem::FILE_NAME_INFO>();
            let info = &body[0];
            let name_len = info.FileNameLength as usize / size_of::<u16>();
            let name_u16 = std::slice::from_raw_parts(info.FileName.as_ptr() as *const u16, name_len);
            let path = String::from_utf16(name_u16).unwrap_or(String::from(""));
            return path;
        }

        return String::from("");
    }
}

pub struct SearchResultItem {
    pub path: String,
    pub file_name: String,
    pub rank: i8,
}

impl Clone for SearchResultItem {
    fn clone(&self) -> Self {
        SearchResultItem {
            path: self.path.clone(),
            file_name: self.file_name.clone(),
            rank: self.rank,
        }
    }
}

pub struct SearchResult {
    pub items: Vec<SearchResultItem>,
    pub query: String,
}

pub struct Volume {
    pub drive: char,
    drive_frn: u64,
    ujd: Ioctl::USN_JOURNAL_DATA_V0,
    start_usn: i64,
    file_map: FileMap,
    file_index: FileIndex,
    stop_receiver: mpsc::Receiver<()>,
    last_query: String,
    last_search_num: usize,
}

impl Volume {
    pub fn new(drive: char, stop_receiver: mpsc::Receiver<()>) -> Volume {
        let file_path = env::current_exe().unwrap().parent().unwrap().join("userdata").join(drive.to_string());

        Volume {
            drive,
            drive_frn: 0x5000000000005,
            file_map: FileMap::new(),
            file_index: FileIndex::new(file_path.to_str().unwrap_or("")),
            start_usn: 0x0,
            ujd: Ioctl::USN_JOURNAL_DATA_V0{ UsnJournalID: 0x0, FirstUsn: 0x0, NextUsn: 0x0, LowestValidUsn: 0x0, MaxUsn: 0x0, MaximumSize: 0x0, AllocationDelta: 0x0 },
            stop_receiver,
            last_query: String::new(),
            last_search_num: 0,
        }
    }

    // This is a helper function that opens a handle to the volume specified by the cDriveLetter parameter.
    fn open_drive(drive_letter: char) -> isize {
        unsafe{
            let c_str: CString = CString::new(format!("\\\\.\\{}:", drive_letter)).unwrap();
            FileSystem::CreateFileA(
                c_str.as_ptr() as *const u8, 
                Foundation::GENERIC_READ,
                FileSystem::FILE_SHARE_READ | FileSystem::FILE_SHARE_WRITE, 
                std::ptr::null::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>(), 
                FileSystem::OPEN_EXISTING, 
                0, 
                0)
        }
    }

    // This is a helper function that close a handle.
    fn close_drive(h_vol: isize) {
        unsafe { Foundation::CloseHandle(h_vol); }
    }

    // Enumerate the MFT for all entries. Store the file reference numbers of any directories in the database.
    pub fn build_index(&mut self) {
        #[cfg(debug_assertions)]
        let sys_time = SystemTime::now();
        #[cfg(debug_assertions)]
        log_info(format!("{} Begin Volume::build_index", self.drive));

        self.release_index();

        let h_vol = Self::open_drive(self.drive);

        // Query, Return statistics about the journal on the current volume
        let mut cd: u32 = 0;
        unsafe { 
            IO::DeviceIoControl(
                h_vol, 
                Ioctl::FSCTL_QUERY_USN_JOURNAL, 
                std::ptr::null(), 
                0, 
                &mut self.ujd as *mut Ioctl::USN_JOURNAL_DATA_V0 as *mut c_void, 
                std::mem::size_of::<Ioctl::USN_JOURNAL_DATA_V0>().try_into().unwrap(), 
                &mut cd, 
                std::ptr::null::<IO::OVERLAPPED>() as *mut IO::OVERLAPPED
            )
        };

        self.start_usn = self.ujd.NextUsn;

        // add the root directory
        let sz_root = format!("{}:", self.drive);
        self.file_map.insert(self.drive_frn, sz_root, 0);

        let mut med: Ioctl::MFT_ENUM_DATA_V0 = Ioctl::MFT_ENUM_DATA_V0 {
            StartFileReferenceNumber: 0,
            LowUsn: 0,
            HighUsn: self.ujd.NextUsn,
        };
        let mut data = [0u64; 0x10000];
        let mut cb: u32 = 0;
        
        unsafe{
            while IO::DeviceIoControl(
                h_vol, 
                Ioctl::FSCTL_ENUM_USN_DATA, 
                &med as *const _ as *const c_void, 
                std::mem::size_of::<Ioctl::MFT_ENUM_DATA_V0>() as u32, 
                data.as_mut_ptr() as *mut c_void, 
                std::mem::size_of::<[u8; std::mem::size_of::<u64>() * 0x10000]>() as u32, 
                &mut cb as *mut u32, 
                std::ptr::null_mut()
            ) != 0 {
                let mut record_ptr = data.as_ptr().offset(1) as *const Ioctl::USN_RECORD_V2;
                let data_end = data.as_ptr() as usize + cb as usize;

                while (record_ptr as usize) < data_end {
                    let record = &*record_ptr;
                    let file_name_begin_ptr = (record_ptr as usize + record.FileNameOffset as usize) as *const u16;
                    let file_name_length = record.FileNameLength as usize / std::mem::size_of::<u16>();
                    let file_name_list = std::slice::from_raw_parts(file_name_begin_ptr, file_name_length);
                    let file_name = String::from_utf16(file_name_list).unwrap_or(String::from("unknown"));

                    let file_path = get_file_path(h_vol, record.ParentFileReferenceNumber);
                    let file_full_path = format!("{}:{}\\{}", self.drive, file_path, file_name);
                    self.file_index.add(file_name, file_full_path, false, "".to_string());

                    // self.file_map.insert(record.FileReferenceNumber, file_name, record.ParentFileReferenceNumber);
                    record_ptr = (record_ptr as usize + record.RecordLength as usize) as *mut Ioctl::USN_RECORD_V2;
                }

                med.StartFileReferenceNumber = data[0];
            }
        }

        #[cfg(debug_assertions)]
        log_info(format!("{} End Volume::build_index, use time: {:?} ms", self.drive, sys_time.elapsed().unwrap().as_millis()));
        
        Self::close_drive(h_vol);
        self.serialization_write().unwrap_or_else(|err: io::Error| {
            log_error(format!("{} Volume::serialization_write, error: {:?}", self.drive, err));
        });
    }

    // Clears the database
    pub fn release_index(&mut self) {
        if self.file_map.is_empty() {return;}

        self.last_query = String::new();
        self.last_search_num = 0;

        #[cfg(debug_assertions)]
        log_info(format!("{} Begin Volume::release_index", self.drive));

        self.file_map.clear();
    }

    // return true if contain query
    fn match_str(contain: &str, query_lower: &String) -> bool {
        let lower_contain = contain.to_lowercase();
        for s in query_lower.split('*') { // for wildcard
            if !lower_contain.contains(s) {
                return false;
            }
        }
        true
    }

    // searching
    pub fn find(&mut self, query: String, batch: u8, sender: mpsc::Sender<Option<Vec<SearchResultItem>>>) {
        #[cfg(debug_assertions)]
        let sys_time = SystemTime::now();

        #[cfg(debug_assertions)]
        log_info(format!("{} Begin Volume::Find {query}", self.drive));

        if query.is_empty() { let _ = sender.send(None); return;}
        // if self.file_map.is_empty() { 
        //     self.serialization_read().unwrap_or_else(|err: Box<dyn Error>| {
        //         log_error(format!("{} Volume::serialization_write, error: {:?}", self.drive, err));
        //         self.build_index();
        //     });
        // };
        
        while self.stop_receiver.try_recv().is_ok() { } // clear channel before find
        
        let mut result = Vec::new();
        let mut find_num = 0;
        let mut search_num: usize = 0;
        let query_lower = query.to_lowercase();
        // let query_filter = make_filter(&query_lower);
        // if self.last_query != query {
        //     self.last_search_num = 0;
        //     self.last_query = query.clone();
        // }

        let index_result: Vec<file_index::FileView> = self.file_index.search(query_lower, 10);
        for item in index_result {
            result.push(SearchResultItem {
                path: item.abs_path,
                file_name: item.name,
                rank: 0,
            });
        }

        // let file_map_iter = self.file_map.iter().rev().skip(self.last_search_num);
        // for (_, file) in file_map_iter {
        //     if self.stop_receiver.try_recv().is_ok() {
        //         #[cfg(debug_assertions)]
        //         log_info(format!("{} Stop Volume::Find", self.drive));
        //         let _ = sender.send(None);
        //         return;
        //     }
        //     search_num += 1;
        //     if (file.filter & query_filter) == query_filter && Self::match_str(&file.file_name, &query_lower) {
        //         if let Some(path) = self.file_map.get_path(&file.parent_index) {
        //             result.push(SearchResultItem {
        //                 path,
        //                 file_name: file.file_name.clone(),
        //                 rank: file.rank,
        //             });
        //             find_num += 1;
        //             if find_num >= batch { break; }
        //         }
        //     }
        // }

        #[cfg(debug_assertions)]
        log_info(format!("{} End Volume::Find {query}, use time: {:?} ms, get result num {}", self.drive, sys_time.elapsed().unwrap().as_millis(), result.len()));
        
        self.last_search_num += search_num;
        let _ = sender.send(Some(result));
    }

    // update index, add new file, remove deleted file
    pub fn update_index(&mut self) {
        #[cfg(debug_assertions)]
        log_info(format!("{} Begin Volume::update_index", self.drive));

        if self.file_map.is_empty() { 
            self.serialization_read().unwrap_or_else(|err: Box<dyn Error>| {
                log_error(format!("{} Volume::serialization_write, error: {:?}", self.drive, err));
                self.build_index();
            });
        };

        let mut data = [0i64; 0x10000];
        let mut cb: u32 = 0;
        let mut rujd: Ioctl::READ_USN_JOURNAL_DATA_V0 = Ioctl::READ_USN_JOURNAL_DATA_V0 {
                StartUsn: self.start_usn,
                ReasonMask: Ioctl::USN_REASON_FILE_CREATE | Ioctl::USN_REASON_FILE_DELETE | Ioctl::USN_REASON_RENAME_NEW_NAME | Ioctl::USN_REASON_RENAME_OLD_NAME,
                ReturnOnlyOnClose: 0,
                Timeout: 0,
                BytesToWaitFor: 0,
                UsnJournalID: self.ujd.UsnJournalID,
        };

        let h_vol = Self::open_drive(self.drive);

        unsafe{
            while IO::DeviceIoControl(
                h_vol, 
                Ioctl::FSCTL_READ_USN_JOURNAL, 
                &rujd as *const _ as *const c_void,
                std::mem::size_of::<Ioctl::READ_USN_JOURNAL_DATA_V0>().try_into().unwrap(), 
                data.as_mut_ptr() as *mut c_void, 
                std::mem::size_of::<[u8; std::mem::size_of::<u64>() * 0x10000]>() as u32, 
                &mut cb as *mut u32, 
                std::ptr::null_mut()
            ) != 0 {
                if cb == 8 { break };
                let mut record_ptr = data.as_ptr().offset(1) as *const Ioctl::USN_RECORD_V2;
                let data_end = data.as_ptr() as usize + cb as usize;
                
                while (record_ptr as usize) < data_end {
                    let record = &*record_ptr;
                    let file_name_begin_ptr = (record_ptr as usize + record.FileNameOffset as usize) as *const u16;
                    let file_name_length = record.FileNameLength as usize / std::mem::size_of::<u16>();
                    let file_name_list = std::slice::from_raw_parts(file_name_begin_ptr, file_name_length);
                    let file_name = String::from_utf16(file_name_list).unwrap_or(String::from("unknown"));
                    
                    if record.Reason & (Ioctl::USN_REASON_FILE_CREATE | Ioctl::USN_REASON_RENAME_NEW_NAME) != 0 {
                        self.file_map.insert(record.FileReferenceNumber, file_name, record.ParentFileReferenceNumber);
                    } else { // Ioctl::USN_REASON_FILE_DELETE | Ioctl::USN_REASON_RENAME_OLD_NAME
                        self.file_map.remove(&record.FileReferenceNumber);
                    }

                    record_ptr = (record_ptr as usize + record.RecordLength as usize) as *mut Ioctl::USN_RECORD_V2;
                }
                
                rujd.StartUsn = data[0];
            }
        }
        self.start_usn = rujd.StartUsn;
        Self::close_drive(h_vol);
    }

    // serializate file_map to reduce memory usage
    fn serialization_write(&mut self) -> Result<(), io::Error> {
        #[cfg(debug_assertions)]
        let sys_time = SystemTime::now();
        #[cfg(debug_assertions)]
        log_info(format!("{} Begin Volume::serialization_write", self.drive));

        if self.file_map.is_empty() {return Ok(())};
        
        let file_path = env::current_exe().unwrap().parent().unwrap().join("userdata");
        if !file_path.exists() { fs::create_dir(&file_path)?; }

        let mut save_file = fs::File::create(format!("{}/{}.fd", file_path.to_str().unwrap(), self.drive))?;

        let mut buf = Vec::new();
        buf.write_all(&self.start_usn.to_be_bytes())?;
        for (file_key, file) in self.file_map.iter() {
            buf.write_all(&file_key.index.to_be_bytes())?;
            buf.write_all(&file.parent_index.to_be_bytes())?;
            buf.write_all(&(file.file_name.len() as u16).to_be_bytes())?;
            buf.write_all(file.file_name.as_bytes())?;
            buf.write_all(&file.filter.to_be_bytes())?;
            buf.write_all(&file.rank.to_be_bytes())?;
        }
        let _ = save_file.write(&buf.to_vec());
        self.release_index();

        #[cfg(debug_assertions)]
        log_info(format!("{} End Volume::serialization_write, use time: {:?} ms", self.drive, sys_time.elapsed().unwrap().as_millis()));

        Ok(())
    }

    // deserializate file_map from file
    fn serialization_read(&mut self) -> Result<(), Box<dyn Error>> {
        #[cfg(debug_assertions)]
        let sys_time = SystemTime::now();
        #[cfg(debug_assertions)]
        log_info(format!("{} Begin Volume::serialization_read", self.drive));
        
        let file_path = env::current_exe().unwrap().parent().unwrap().join("userdata");
        let file_path_str = file_path.to_str().unwrap();
        
        let file_data = fs::read(format!("{}/{}.fd", file_path_str, self.drive))?;

        if file_data.len() < 8 { return Err(io::Error::new(io::ErrorKind::InvalidData, "File data too short.").into()); }

        self.start_usn = i64::from_be_bytes(file_data[0..8].try_into()?);
        let mut ptr_index = 8;

        while ptr_index < file_data.len() {
            if ptr_index + 18 > file_data.len() { return Err(io::Error::new(io::ErrorKind::InvalidData, "File data size error.").into()); }
            
            let index = u64::from_be_bytes(file_data[ptr_index..ptr_index+8].try_into()?);
            ptr_index += 8;
            let parent_index = usize::from_be_bytes(file_data[ptr_index..ptr_index+8].try_into()?) as u64;
            ptr_index += 8;
            let file_name_len = u16::from_be_bytes(file_data[ptr_index..ptr_index+2].try_into()?) as u16;
            ptr_index += 2;

            if ptr_index + (file_name_len as usize) + 5 > file_data.len() { return Err(io::Error::new(io::ErrorKind::InvalidData, "File data size error.").into()); }

            let file_name = String::from_utf8(file_data[ptr_index..(ptr_index + file_name_len as usize)].to_vec())?;
            ptr_index += file_name_len as usize;
            let filter = u32::from_be_bytes(file_data[ptr_index..ptr_index+4].try_into()?);
            ptr_index += 4;
            let rank = i8::from_be_bytes(file_data[ptr_index..ptr_index+1].try_into()?);
            ptr_index += 1;
            self.file_map.insert_simple(index, File { parent_index, file_name, filter, rank });
        }

        #[cfg(debug_assertions)]
        log_info(format!("{} End Volume::serialization_read, use time: {:?} ms", self.drive, sys_time.elapsed().unwrap().as_millis()));

        Ok(())
    }
}