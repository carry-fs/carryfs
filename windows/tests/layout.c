/* Prints WinFsp struct layouts for windows/tests/layout.sh. */
#include <windows.h>
#undef _ReadWriteBarrier /* mingw defines it as a macro; fsctl.h uses the name. */
#include <winfsp/winfsp.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>

#define SIZE(name, type) printf("%s %zu\n", name, sizeof(type))
#define OFFSET(name, type, field) printf("%s %zu\n", name, offsetof(type, field))
#define FLAG(name, field)                                                     \
    do {                                                                      \
        FSP_FSCTL_VOLUME_PARAMS p;                                            \
        UINT32 word;                                                          \
        memset(&p, 0, sizeof p);                                              \
        p.field = 1;                                                          \
        memcpy(&word, (char *)&p + offsetof(FSP_FSCTL_VOLUME_PARAMS, FileInfoTimeout) + 4, 4); \
        printf("FLAG.%s %u\n", name, word);                                   \
    } while (0)
/* The V1 bit-field word sits just before VolumeInfoTimeout. */
#define FLAG1(name, field)                                                    \
    do {                                                                      \
        FSP_FSCTL_VOLUME_PARAMS p;                                            \
        UINT32 word;                                                          \
        memset(&p, 0, sizeof p);                                              \
        p.field = 1;                                                          \
        memcpy(&word, (char *)&p + offsetof(FSP_FSCTL_VOLUME_PARAMS, VolumeInfoTimeout) - 4, 4); \
        printf("FLAG1.%s %u\n", name, word);                                  \
    } while (0)

int main(void)
{
    SIZE("FILE_INFO", FSP_FSCTL_FILE_INFO);
    SIZE("VOLUME_PARAMS", FSP_FSCTL_VOLUME_PARAMS);
    OFFSET("VP.FileInfoTimeout", FSP_FSCTL_VOLUME_PARAMS, FileInfoTimeout);
    OFFSET("VP.Prefix", FSP_FSCTL_VOLUME_PARAMS, Prefix);
    OFFSET("VP.FileSystemName", FSP_FSCTL_VOLUME_PARAMS, FileSystemName);
    SIZE("OPEN_FILE_INFO", FSP_FSCTL_OPEN_FILE_INFO);
    OFFSET("OFI.NormalizedName", FSP_FSCTL_OPEN_FILE_INFO, NormalizedName);
    OFFSET("OFI.NormalizedNameSize", FSP_FSCTL_OPEN_FILE_INFO, NormalizedNameSize);
    SIZE("VOLUME_INFO", FSP_FSCTL_VOLUME_INFO);
    OFFSET("DIR_INFO.FileNameBuf", FSP_FSCTL_DIR_INFO, FileNameBuf);
    SIZE("INTERFACE", FSP_FILE_SYSTEM_INTERFACE);
    OFFSET("INTERFACE.ReadDirectory", FSP_FILE_SYSTEM_INTERFACE, ReadDirectory);
    OFFSET("INTERFACE.ResolveReparsePoints", FSP_FILE_SYSTEM_INTERFACE, ResolveReparsePoints);
    OFFSET("INTERFACE.DeleteReparsePoint", FSP_FILE_SYSTEM_INTERFACE, DeleteReparsePoint);
    OFFSET("FILE_SYSTEM.UserContext", FSP_FILE_SYSTEM, UserContext);
    OFFSET("REQ.TargetOnFileSystem", FSP_FSCTL_TRANSACT_REQ, Req.FileSystemControl.TargetOnFileSystem);
    OFFSET("CONTEXT.Response", FSP_FILE_SYSTEM_OPERATION_CONTEXT, Response);
    SIZE("NOTIFY_INFO", FSP_FSCTL_NOTIFY_INFO);
    {
        /* DisableCache is a bit-field: find the word it sets and the bit. */
        FSP_FSCTL_TRANSACT_RSP r;
        UINT32 word = 0;
        size_t at;
        memset(&r, 0, sizeof r);
        r.Rsp.Create.Opened.DisableCache = 1;
        for (at = 0; at + 4 <= sizeof r && 0 == word; at += 4)
            memcpy(&word, (char *)&r + at, 4);
        printf("RSP.OpenedFlags %zu\n", at - 4);
        printf("RSP.DisableCache %u\n", word);
    }
    FLAG("CasePreservedNames", CasePreservedNames);
    FLAG("UnicodeOnDisk", UnicodeOnDisk);
    FLAG("ReparsePoints", ReparsePoints);
    FLAG("ReadOnlyVolume", ReadOnlyVolume);
    FLAG("UmFileContextIsUserContext2", UmFileContextIsUserContext2);
    FLAG("PostDispositionWhenNecessaryOnly", PostDispositionWhenNecessaryOnly);
    OFFSET("VP.VolumeInfoTimeout", FSP_FSCTL_VOLUME_PARAMS, VolumeInfoTimeout);
    OFFSET("VP.SecurityTimeout", FSP_FSCTL_VOLUME_PARAMS, SecurityTimeout);
    FLAG1("VolumeInfoTimeoutValid", VolumeInfoTimeoutValid);
    FLAG1("DirInfoTimeoutValid", DirInfoTimeoutValid);
    FLAG1("SecurityTimeoutValid", SecurityTimeoutValid);
    return 0;
}
