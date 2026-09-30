#include "FileDropService.h"
#include <algorithm>

std::vector<std::wstring> FileDropService::FilterAudioFiles(
    const std::vector<std::wstring>& files)
{
    std::vector<std::wstring> result;

    for (const auto& file : files)
    {
        auto pos = file.find_last_of(L'.');
        if (pos == std::wstring::npos)
            continue;

        auto ext = file.substr(pos);

        if (ext == L".wav" ||
            ext == L".mp3" ||
            ext == L".flac" ||
            ext == L".m4a")
        {
            result.push_back(file);
        }
    }

    return result;
}
