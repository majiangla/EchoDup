#pragma once

#include <vector>
#include <functional>
#include "../Model/AudioFile.h"
#include "ScanTask.h"

namespace EchoDup::Core
{
class ScanService
{
public:
    void AddTask(const ScanTask& task);
    void Start(const std::vector<AudioFile>& files);
    void Cancel();

private:
    std::vector<ScanTask> tasks;
    bool cancelled{false};
};
}
