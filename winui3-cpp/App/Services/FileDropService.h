#pragma once

#include <string>
#include <vector>

class FileDropService
{
public:
    std::vector<std::wstring> FilterAudioFiles(
        const std::vector<std::wstring>& files);
};
