#pragma once

#include <vector>
#include <string>

namespace EchoDup::Core
{
class ScanPipeline
{
public:
    void AddFile(std::wstring path);
    void Start();
    void Cancel();

private:
    std::vector<std::wstring> files_;
};
}
