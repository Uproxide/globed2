#include "invite_popup.hpp"

#include "player_list_cell.hpp"
#include "room_join_popup.hpp"
#include "room_settings_popup.hpp"
#include <data/packets/all.hpp>
#include <net/network_manager.hpp>
#include <managers/error_queues.hpp>
#include <managers/profile_cache.hpp>
#include <managers/friend_list.hpp>
#include <managers/settings.hpp>
#include <managers/room.hpp>
#include <ui/general/ask_input_popup.hpp>
#include <util/ui.hpp>
#include <util/misc.hpp>
#include <util/format.hpp>

using namespace geode::prelude;

bool InvitePopup::setup() {
    auto& nm = NetworkManager::get();
    if (!nm.established()) {
        return false;
    }

    FriendListManager::get().maybeLoad();

    auto& rm = RoomManager::get();

    nm.addListener<GlobalPlayerListPacket>(this, [this](std::shared_ptr<GlobalPlayerListPacket> packet) {
        this->isWaiting = false;
        this->playerList = packet->data;
        this->applyFilter("");
        this->sortPlayerList();
        this->onLoaded(!roomBtnMenu);
    });

    auto popupLayout = util::ui::getPopupLayout(m_size);

    auto listview = ListView::create(CCArray::create(), PlayerListCell::CELL_HEIGHT, LIST_WIDTH, LIST_HEIGHT);
    listLayer = GJCommentListLayer::create(listview, "", util::ui::BG_COLOR_BROWN, LIST_WIDTH, LIST_HEIGHT, false);

    float xpos = (m_mainLayer->getScaledContentSize().width - LIST_WIDTH) / 2;
    listLayer->setPosition({xpos, 85.f});
    m_mainLayer->addChild(listLayer);

    this->reloadPlayerList();

    Build<CCSprite>::createSpriteName("GJ_updateBtn_001.png")
        .scale(0.9f)
        .intoMenuItem([this](auto) {
            this->reloadPlayerList(true);
        })
        .pos(m_size.width / 2.f - 3.f, -m_size.height / 2.f + 3.f)
        .id("reload-btn"_spr)
        .intoNewParent(CCMenu::create())
        .parent(m_mainLayer);

    Build<CCMenu>::create()
        .layout(ColumnLayout::create()->setGap(1.f)->setAxisAlignment(AxisAlignment::End)->setAxisReverse(true))
        .scale(0.875f)
        .pos(popupLayout.right - 6.f, popupLayout.top - 6.f)
        .anchorPoint(1.f, 1.f)
        .contentSize(30.f, POPUP_HEIGHT)
        .parent(m_mainLayer)
        .id("top-right-buttons"_spr)
        .store(buttonMenu);

    CCMenuItemSpriteExtra *filterBtn;

    // search button
    Build<CCSprite>::createSpriteName("gj_findBtn_001.png")
        .intoMenuItem([this](auto) {
            AskInputPopup::create("Search Player", [this](const std::string_view input) {
                this->applyFilter(input);
                this->sortPlayerList();
                this->onLoaded(true);
            }, 16, "Username", util::misc::STRING_ALPHANUMERIC, 3.f)->show();
        })
        .scaleMult(1.1f)
        .id("search-btn"_spr)
        .parent(buttonMenu)
        .store(filterBtn);

    // clear search button
    Build<CCSprite>::createSpriteName("gj_findBtnOff_001.png")
        .intoMenuItem([this](auto) {
            this->applyFilter("");
            this->sortPlayerList();
            this->onLoaded(true);
        })
        .scaleMult(1.1f)
        .id("search-clear-btn"_spr)
        .store(clearSearchButton);

    buttonMenu->updateLayout();

    this->scheduleUpdate();

    return true;
}

void InvitePopup::update(float) {
    //settingsButton->setVisible(RoomManager::get().isInRoom());
}

void InvitePopup::onLoaded(bool stateChanged) {
    this->removeLoadingCircle();

    auto cells = CCArray::create();

    for (const auto& pdata : filteredPlayerList) {
        auto* cell = PlayerListCell::create(pdata.makeRoomPreview(), true);
        cells->addObject(cell);
    }

    // preserve scroll position
    float scrollPos = util::ui::getScrollPos(listLayer->m_list);
    int previousCellCount = listLayer->m_list->m_entries->count();

    listLayer->m_list->removeFromParent();
    listLayer->m_list = Build<ListView>::create(cells, PlayerListCell::CELL_HEIGHT, LIST_WIDTH, LIST_HEIGHT)
        .parent(listLayer)
        .collect();

    if (previousCellCount != 0 && !stateChanged) {
        util::ui::setScrollPos(listLayer->m_list, scrollPos);
    }

    if (stateChanged) {
        this->addButtons();
    }

    buttonMenu->updateLayout();
}

void InvitePopup::addButtons() {
    // remove existing buttons
    if (roomBtnMenu) roomBtnMenu->removeFromParent();

    auto& rm = RoomManager::get();
    float popupCenter = CCDirector::get()->getWinSize().width / 2;

    Build<CCMenu>::create()
        .layout(
            RowLayout::create()
            ->setAxisAlignment(AxisAlignment::Center)
            ->setGap(5.0f)
        )
        .id("btn-menu"_spr)
        .pos(popupCenter, 55.f)
        .parent(m_mainLayer)
        .store(roomBtnMenu);
}

void InvitePopup::removeLoadingCircle() {
    if (loadingCircle) {
        loadingCircle->fadeAndRemove();
        loadingCircle = nullptr;
    }
}

void InvitePopup::reloadPlayerList(bool sendPacket) {
    auto& nm = NetworkManager::get();
    if (!nm.established()) {
        this->onClose(this);
        return;
    }

    // remove loading circle
    this->removeLoadingCircle();

    // send the request
    if (sendPacket) {
        if (!isWaiting) {
            NetworkManager::get().send(RequestGlobalPlayerListPacket::create());
            isWaiting = true;
        }
    }

    // show the circle
    loadingCircle = LoadingCircle::create();
    loadingCircle->setParentLayer(listLayer);
    loadingCircle->setPosition(-listLayer->getPosition());
    loadingCircle->show();
}

bool InvitePopup::isLoading() {
    return loadingCircle != nullptr;
}

void InvitePopup::sortPlayerList() {
    auto& flm = FriendListManager::get();

    // filter out the weird people (old game server used to send unauthenticated people too)
    filteredPlayerList.erase(std::remove_if(filteredPlayerList.begin(), filteredPlayerList.end(), [](const auto& player) {
        return player.accountId == 0;
    }), filteredPlayerList.end());

    // show friends before everyone else, and sort everyone alphabetically by the name
    std::sort(filteredPlayerList.begin(), filteredPlayerList.end(), [&flm](const auto& p1, const auto& p2) -> bool {
        bool isFriend1 = flm.isFriend(p1.accountId);
        bool isFriend2 = flm.isFriend(p2.accountId);

        if (isFriend1 != isFriend2) {
            return isFriend1;
        } else {
            // convert both names to lowercase
            std::string name1 = p1.name, name2 = p2.name;
            std::transform(name1.begin(), name1.end(), name1.begin(), ::tolower);
            std::transform(name2.begin(), name2.end(), name2.begin(), ::tolower);

            // sort alphabetically
            return name1 < name2;
        }
    });
}

void InvitePopup::applyFilter(const std::string_view input) {
    filteredPlayerList.clear();

    if (input.empty()) {
        for (const auto& item : playerList) {
            filteredPlayerList.push_back(item);
        }

        clearSearchButton->removeFromParent();
        return;
    }

    auto filt = util::format::toLowercase(input);

    for (const auto& item : playerList) {
        auto name = util::format::toLowercase(item.name);
        if (name.find(filt) != std::string::npos) {
            filteredPlayerList.push_back(item);
        }
    }
    buttonMenu->addChild(clearSearchButton);
    buttonMenu->updateLayout();
}

void InvitePopup::setRoomTitle() {
    auto layout = util::ui::getPopupLayout(m_size);

    CCNode* elem = Build<CCLabelBMFont>::create("Invite Player", "goldFont.fnt")
        .scale(0.7f)
        .collect();

    elem->setPosition(layout.centerTop - CCPoint{0.f, 17.f});
    m_mainLayer->addChild(elem);
}

InvitePopup* InvitePopup::create() {
    auto ret = new InvitePopup;
    if (ret->init(POPUP_WIDTH, POPUP_HEIGHT)) {
        ret->autorelease();
        return ret;
    }

    delete ret;
    return nullptr;
}