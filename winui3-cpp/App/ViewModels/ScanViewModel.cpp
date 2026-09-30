#include "ScanViewModel.h"

namespace EchoDup::UI {

void ScanViewModel::AddFile(const std::wstring& path)
{
    EchoDup::Core::AudioFile file{};
    file.path = path;
    files.push_back(file);
}

void ScanViewModel::StartScan()
{
    // Connect to ScanService in next stage.
}

}
