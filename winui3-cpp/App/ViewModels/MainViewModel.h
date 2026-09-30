#pragma once
#include <string>
#include <vector>

namespace EchoDup::UI {

class MainViewModel
{
public:
    void AddFile(const std::wstring& path);
    const std::vector<std::wstring>& Files() const;

private:
    std::vector<std::wstring> m_files;
};

}
