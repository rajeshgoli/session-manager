/* Only native, unsigned or ad-hoc executables are proven to admit this
 * ad-hoc adapter. Platform, restricted, hardened and library-validated
 * executables cannot inherit adapted listeners. Signature layout/flags:
 * https://github.com/apple-oss-distributions/xnu/blob/main/osfmk/kern/cs_blobs.h */
#include <mach-o/loader.h>
#include <mach-o/fat.h>
#include <sys/stat.h>
#include <limits.h>
extern const char *const wall_executable_roots[];

static int immutable_executable(const char *path, char canonical[PATH_MAX]) {
    if (!realpath(path, canonical)) {
        // Darwin can report EINVAL when a symlink changes during resolution.
        // An unverifiable inheritance path is refused, never retried by name.
        if (errno == ENOENT || errno == ENOTDIR) return -1;
        return failure(EACCES);
    }
    for (unsigned i = 0; wall_executable_roots[i]; ++i) {
        size_t length = strlen(wall_executable_roots[i]);
        if (!strncmp(canonical, wall_executable_roots[i], length) && canonical[length] == '/') return 0;
    }
    return failure(EACCES);
}

static int read_exact(int fd, void *bytes, size_t length, off_t offset) {
    size_t done = 0;
    while (done < length) {
        ssize_t count = pread(fd, (char *)bytes + done, length - done, offset + (off_t)done);
        if (count < 0 && errno == EINTR) continue;
        if (count <= 0) return failure(EACCES);
        done += (size_t)count;
    }
    return 0;
}
static uint32_t image_word(const unsigned char *bytes) {
    return (uint32_t)bytes[0] << 24 | (uint32_t)bytes[1] << 16 |
        (uint32_t)bytes[2] << 8 | bytes[3];
}
static int signature_permits(int fd, off_t offset, uint32_t size) {
    if (size < 12 || size > 2 * 1024 * 1024) return 0;
    unsigned char *bytes = malloc(size);
    if (!bytes) return 0;
    int allowed = 0;
    if (read_exact(fd, bytes, size, offset) < 0 || image_word(bytes) != 0xfade0cc0 ||
        image_word(bytes + 4) > size) goto done;
    uint32_t length = image_word(bytes + 4), count = image_word(bytes + 8);
    if (length < 12 || count > (length - 12) / 8) goto done;
    int directories = 0;
    for (uint32_t i = 0; i < count; ++i) {
        uint32_t type = image_word(bytes + 12 + i * 8);
        if (type != 0 && (type < 0x1000 || type > 0x1005)) continue;
        uint32_t position = image_word(bytes + 16 + i * 8);
        if (position > length || length - position < 16 ||
            image_word(bytes + position) != 0xfade0c02) goto done;
        uint32_t flags = image_word(bytes + position + 12);
        /* CS_ADHOC=2; reject CS_RESTRICT, CS_REQUIRE_LV, CS_RUNTIME,
         * CS_PLATFORM_BINARY and CS_PLATFORM_PATH. */
        if (!(flags & 2) || (flags & (0x800 | 0x2000 | 0x10000 | 0x4000000 | 0x8000000))) goto done;
        ++directories;
    }
    allowed = directories > 0;
done:
    free(bytes);
    return allowed;
}
static int image_permits(const char *path) {
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) return -1;
    struct stat status;
    struct mach_header_64 header;
    off_t slice = 0;
    int allowed = 0;
    if (fstat(fd, &status) < 0 || !S_ISREG(status.st_mode) ||
        (status.st_mode & (S_ISUID | S_ISGID)) || (status.st_flags & SF_RESTRICTED) ||
        read_exact(fd, &header, sizeof(header), 0) < 0) goto done;
    if (header.magic == FAT_CIGAM) {
        struct fat_header fat;
        memcpy(&fat, &header, sizeof(fat));
        uint32_t count = ntohl(fat.nfat_arch);
        if (!count || count > 64) goto done;
        int found = 0;
        for (uint32_t i = 0; i < count; ++i) {
            struct fat_arch architecture;
            if (read_exact(fd, &architecture, sizeof(architecture), sizeof(fat) + i * sizeof(architecture)) < 0) goto done;
#if defined(__arm64__)
            const uint32_t cpu = CPU_TYPE_ARM64;
#else
            const uint32_t cpu = CPU_TYPE_X86_64;
#endif
            if (ntohl(architecture.cputype) == cpu) { slice = ntohl(architecture.offset); found = 1; break; }
        }
        if (!found || read_exact(fd, &header, sizeof(header), slice) < 0) goto done;
    }
    if (header.magic != MH_MAGIC_64 || header.filetype != MH_EXECUTE || !(header.flags & MH_DYLDLINK) ||
        header.sizeofcmds > 1024 * 1024 || header.ncmds > header.sizeofcmds / sizeof(struct load_command)) goto done;
#if defined(__arm64__)
    if (header.cputype != CPU_TYPE_ARM64) goto done;
#else
    if (header.cputype != CPU_TYPE_X86_64) goto done;
#endif
    off_t position = slice + sizeof(header), end = position + header.sizeofcmds;
    allowed = 1;
    for (uint32_t i = 0; i < header.ncmds; ++i) {
        struct load_command command;
        if (read_exact(fd, &command, sizeof(command), position) < 0 ||
            command.cmdsize < sizeof(command) || command.cmdsize > end - position) { allowed = 0; break; }
        if (command.cmd == LC_CODE_SIGNATURE) {
            struct linkedit_data_command signature;
            if (command.cmdsize < sizeof(signature) || read_exact(fd, &signature, sizeof(signature), position) < 0 ||
                !signature_permits(fd, slice + signature.dataoff, signature.datasize)) { allowed = 0; break; }
        }
        if (command.cmd == LC_SEGMENT_64) {
            struct segment_command_64 segment;
            if (command.cmdsize < sizeof(segment) || read_exact(fd, &segment, sizeof(segment), position) < 0 ||
                !strncmp(segment.segname, "__RESTRICT", sizeof(segment.segname))) { allowed = 0; break; }
        }
        position += command.cmdsize;
    }
done:
    close(fd);
    return allowed ? 0 : failure(EACCES);
}
