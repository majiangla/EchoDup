#pragma once
#include <vector>
#include <string>
#include "../../Core/Model/AudioFile.h"

namespace EchoDup::UI {

class ScanViewModel
{
public:
    void AddFile(const std::wstring& path);
    void StartScan();

private:
    std::vector<EchoDup::Core::AudioFile> files;
};

}
