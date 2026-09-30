#include "ScanService.h"

namespace EchoDup::Core
{
void ScanService::AddTask(const ScanTask& task)
{
    tasks.push_back(task);
}

void ScanService::Start(const std::vector<AudioFile>&)
{
    cancelled = false;
}

void ScanService::Cancel()
{
    cancelled = true;
}
}
