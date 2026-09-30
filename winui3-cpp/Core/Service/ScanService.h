#pragma once
#include <vector>
#include <functional>
#include "../Model/AudioFile.h"

namespace EchoDup::Core {

class ScanService
{
public:
    void Start(const std::vector<AudioFile>& files);
};

}
