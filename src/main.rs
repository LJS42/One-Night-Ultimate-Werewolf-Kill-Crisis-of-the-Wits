use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{timeout, Duration};
use std::sync::Arc;
use std::collections::{HashMap, HashSet};
use rand::{thread_rng, Rng};
use rand::seq::SliceRandom;
use tokio::join;
use tokio::io::BufReader;

#[derive(Debug, Clone)]
struct Player {
    name: String,
    role: String,
    num: usize,
}
#[derive(Debug)]
struct GameState {
    players: HashMap<usize, Player>,
    name_to_id: HashMap<String, usize>,
    player_channels: HashMap<usize, mpsc::Sender<String>>,
    available_roles: Vec<String>,
    night_phase: bool,
    center_cards: Vec<String>,
    used_ids: HashSet<usize>,
    pending_responses: HashMap<usize, oneshot::Sender<String>>,
    available_ids: Vec<usize>,
}


#[tokio::main]
async fn main() {
    let listener = TcpListener::bind("0.0.0.0:8080").await.unwrap();
    let playernum = 6;
    let roles = vec![
        "村民".to_string(), "狼人".to_string(), "爪牙".to_string(), "守夜人".to_string(), 
        "预言家".to_string(), "强盗".to_string(), "女巫".to_string(), "捣蛋鬼".to_string(),
        "酒鬼".to_string(), "失眠者".to_string()
    ];
    
    // 初始化可用的ID
    let ids: Vec<usize> = (1..=playernum).collect();
    
    let game_state = Arc::new(Mutex::new(GameState {
        players: HashMap::new(),
        name_to_id: HashMap::new(),
        player_channels: HashMap::new(),
        available_roles: roles.clone(),
        night_phase: false,
        center_cards: vec![],
        used_ids: HashSet::new(),
        pending_responses: HashMap::new(),
        available_ids: ids,
    }));

    // 创建广播通道
    let (tx, _) = broadcast::channel(100);

    println!("服务器启动，监听 8080 端口...");
    
    while let Ok((stream, _)) = listener.accept().await {
        let game_state = game_state.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            handle_client(stream, game_state, tx).await;
        });
    }
}

async fn handle_client(stream: TcpStream, game_state: Arc<Mutex<GameState>>, tx: broadcast::Sender<String>) {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut buffer = vec![0; 1024];

    let bytes_read = reader.read(&mut buffer).await.unwrap();
    if bytes_read == 0 {
        return;
    }

    let player_name = String::from_utf8_lossy(&buffer[..bytes_read]).to_string();
    let (player_tx, mut player_rx) = mpsc::channel(10);

    let player_id = {
        let mut state = game_state.lock().await;
        if state.available_ids.is_empty() {
            let _ = writer.write_all("游戏已满，无法加入".as_bytes()).await;
            return;
        }
        state.available_ids.pop().unwrap()
    };

    let playernum = {
        let mut state = game_state.lock().await;
        if state.available_roles.is_empty() {
            let _ = writer.write_all("游戏已满，无法加入".as_bytes()).await;
            return;
        }
        let role = state.available_roles.pop().unwrap();
        let new_player = Player {
            name: player_name.clone(),
            role,
            num: player_id,
        };
        state.players.insert(player_id, new_player.clone());
        state.name_to_id.insert(player_name.clone(), player_id);
        state.player_channels.insert(player_id, player_tx.clone());
        state.used_ids.insert(player_id);

        let response = format!("你的 ID: {}，你的身份是: {}", player_id, new_player.role);
        let _ = writer.write_all(response.as_bytes()).await;
        println!("玩家 {} (ID: {}) 加入游戏，身份: {}", player_name, player_id, new_player.role);
        11
    };

    let should_start_game = {
        let state = game_state.lock().await;
        state.players.len() == playernum - 4
    };

    if should_start_game {
        setup_center_cards(game_state.clone()).await;
        start_night_phase(game_state.clone(), tx.clone()).await;
    }

    let mut writer_for_task = writer;
    tokio::spawn(async move {
        while let Some(msg) = player_rx.recv().await {
            let _ = writer_for_task.write_all(msg.as_bytes()).await;
        }
    });

    loop {
        let mut buf = vec![0; 1024];
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                let message = String::from_utf8_lossy(&buf[..n]).to_string();
                process_client_message(player_id, message, game_state.clone()).await;
            }
            Err(_) => break,
        }
    }

    {
        let mut state = game_state.lock().await;
        state.players.remove(&player_id);
        state.player_channels.remove(&player_id);
        if let Some(name) = state.name_to_id.iter().find(|&(_, &id)| id == player_id).map(|(name, _)| name.clone()) {
            state.name_to_id.remove(&name);
        }
        state.available_ids.push(player_id);
        state.used_ids.remove(&player_id);
    }
}

async fn setup_robot(game_state: Arc<Mutex<GameState>>){
    let mut state = game_state.lock().await;
    state.robot = state.available_roles.pop().unwrap();
}

async fn setup_center_cards(game_state: Arc<Mutex<GameState>>) {
    let mut state = game_state.lock().await;
    // 将剩余的角色作为中心牌
    state.center_cards = state.available_roles.clone();
    println!("设置中心牌: {:?}", state.center_cards);
}

async fn send_private_message(game_state: Arc<Mutex<GameState>>, player_id: usize, message: &str) {
    let state = game_state.lock().await;
    if let Some(sender) = state.player_channels.get(&player_id) {
        let _ = sender.send(message.to_string()).await;
    }
}

async fn receive_player_input(player_id: usize, game_state: Arc<Mutex<GameState>>) -> Option<String> {
    let (response_tx, response_rx) = oneshot::channel::<String>();

    {
        let mut state = game_state.lock().await;
        if let Some(old_tx) = state.pending_responses.insert(player_id, response_tx) {
            let _ = old_tx.send("操作已超时".to_string());
        }
        if let Some(sender) = state.player_channels.get(&player_id) {
            let _ = sender.send("请输入你的选择：".to_string()).await;
        }
    }

    match timeout(Duration::from_secs(30), response_rx).await {
        Ok(Ok(input)) => Some(input),
        Ok(Err(_)) => None,
        Err(_) => {
            {
                let mut state = game_state.lock().await;
                state.pending_responses.remove(&player_id);
            }
            let state = game_state.lock().await;
            if let Some(sender) = state.player_channels.get(&player_id) {
                let _ = sender.send("操作超时，已跳过".to_string()).await;
            }
            None
        }
    }
}

async fn process_client_message(player_id: usize, message: String, game_state: Arc<Mutex<GameState>>) {
    let response_tx = {
        let mut state = game_state.lock().await;
        state.pending_responses.remove(&player_id)
    };

    if let Some(tx) = response_tx {
        let _ = tx.send(message);
    } else {
        println!("玩家 {} 发送了消息：{}", player_id, message.trim());
    }
}

async fn start_night_phase(game_state: Arc<Mutex<GameState>>, tx: broadcast::Sender<String>) {
    {
        let mut state = game_state.lock().await;
        state.night_phase = true;
    }
    let _ = tx.send("夜晚开始，所有特殊角色行动。".to_string());

    let ((), ()) = join!(
        handle_werewolves(game_state.clone()),
        handle_minion(game_state.clone())
    );

    handle_nightwatcher(game_state.clone()).await;
    handle_seer(game_state.clone()).await;
    handle_robber(game_state.clone()).await;
    handle_troublemaker(game_state.clone()).await;

    let ((), ()) = join!(
        handle_drunk(game_state.clone()),
        handle_insomniac(game_state.clone())
    );

    handle_robot(game_state.clone());
    handle_robothelper(game_state.clone()).await;

    {
        let mut state = game_state.lock().await;
        state.night_phase = false;
    }

    let _ = tx.send("夜晚结束，天亮了！".to_string());
}


async fn handle_werewolves(game_state: Arc<Mutex<GameState>>) {
    let werewolves: Vec<Player>;
    
    {
        let state = game_state.lock().await;
        werewolves = state.players.values()
            .filter(|p| p.role == "狼人")
            .cloned()
            .collect();
    }
    
    if werewolves.len() > 1 {
        let werewolf_names: Vec<String> = werewolves.iter().map(|w| w.name.clone()).collect();
        for werewolf in &werewolves {
            send_private_message(
                game_state.clone(), 
                werewolf.num, 
                &format!("你的狼人同伴是 {:?}", werewolf_names)
            ).await;
        }
    } else if werewolves.len() == 1 {
        let werewolf = &werewolves[0];
        let center_card;
        
        {
            let state = game_state.lock().await;
            center_card = state.center_cards.choose(&mut thread_rng()).unwrap().clone();
        }
        
        send_private_message(
            game_state.clone(), 
            werewolf.num, 
            &format!("你是独狼，你可以查看一张底牌：{}", center_card)
        ).await;
    }
}

async fn handle_minion(game_state: Arc<Mutex<GameState>>) {
    let minions: Vec<Player>;
    let werewolves: Vec<Player>;
    
    {
        let state = game_state.lock().await;
        minions = state.players.values()
            .filter(|p| p.role == "爪牙")
            .cloned()
            .collect();
        werewolves = state.players.values()
            .filter(|p| p.role == "狼人")
            .cloned()
            .collect();
    }
    
    if !minions.is_empty() {
        for minion in &minions {
            if !werewolves.is_empty() {
                let werewolf_names: Vec<String> = werewolves.iter().map(|w| w.name.clone()).collect();
                send_private_message(
                    game_state.clone(), 
                    minion.num, 
                    &format!("狼人是： {:?}", werewolf_names)
                ).await;
            } else {
                send_private_message(
                    game_state.clone(), 
                    minion.num, 
                    "本局没有狼，所以你现在是狼人。"
                ).await;
            }
        }
    }
}

async fn handle_nightwatcher(game_state: Arc<Mutex<GameState>>) {
    let nightwatchers: Vec<Player>;
    
    {
        let state = game_state.lock().await;
        nightwatchers = state.players.values()
            .filter(|p| p.role == "守夜人")
            .cloned()
            .collect();
    }
    
    if nightwatchers.len() == 1 {
        let nightwatcher = &nightwatchers[0];
        send_private_message(
            game_state.clone(), 
            nightwatcher.num, 
            "只有你一个守夜人噢。"
        ).await;
    } else if nightwatchers.len() > 1 {
        let nightwatcher_names: Vec<String> = nightwatchers.iter().map(|w| w.name.clone()).collect();
        for nightwatcher in &nightwatchers {
            send_private_message(
                game_state.clone(), 
                nightwatcher.num, 
                &format!("你的守夜人同伴是 {:?}", nightwatcher_names)
            ).await;
        }
    }
}

async fn handle_seer(game_state: Arc<Mutex<GameState>>) {
    let seers: Vec<Player>;
    
    {
        let state = game_state.lock().await;
        seers = state.players.values()
            .filter(|p| p.role == "预言家")
            .cloned()
            .collect();
    }
    
    if !seers.is_empty() {
        let seer = &seers[0];
        send_private_message(
            game_state.clone(), 
            seer.num, 
            "选择一个玩家，或者底下两张牌，查看身份。输入玩家编号查看玩家身份，输入 0 查看底下两张牌。"
        ).await;
        
        // 获取预言家的选择
        if let Some(choice) = receive_player_input(seer.num, game_state.clone()).await {
            if let Ok(choice_num) = choice.trim().parse::<usize>() {
                if choice_num == 0 {
                    // 查看底下两张牌
                    let cards: Vec<String>;
                    {
                        let state = game_state.lock().await;
                        // 随机选择两张中心牌
                        let mut indices: Vec<usize> = (0..state.center_cards.len()).collect();
                        indices.shuffle(&mut thread_rng());
                        cards = indices.iter()
                            .take(2)
                            .map(|&i| state.center_cards[i].clone())
                            .collect();
                    }
                    
                    send_private_message(
                        game_state.clone(),
                        seer.num,
                        &format!("底下两张牌是：{} 和 {}", cards[0], cards[1])
                    ).await;
                } else {
                    // 查看指定玩家的身份
                    let role: Option<String>;
                    {
                        let state = game_state.lock().await;
                        role = state.players.get(&choice_num).map(|p| p.role.clone());
                    }
                    
                    if let Some(role) = role {
                        send_private_message(
                            game_state.clone(),
                            seer.num,
                            &format!("玩家 {} 的身份是：{}", choice_num, role)
                        ).await;
                    } else {
                        send_private_message(
                            game_state.clone(),
                            seer.num,
                            "找不到该玩家，请重新选择。"
                        ).await;
                    }
                }
            } else {
                send_private_message(
                    game_state.clone(),
                    seer.num,
                    "输入无效，请输入数字。"
                ).await;
            }
        }
    }
}

async fn handle_robber(game_state: Arc<Mutex<GameState>>) {
    let robbers: Vec<Player> = {
        let state = game_state.lock().await;
        state.players
            .values()
            .filter(|p| p.role == "强盗")
            .cloned()
            .collect()
    };

    if robbers.is_empty() {
        return;
    }

    let robber = &robbers[0];

    send_private_message(
        game_state.clone(),
        robber.num,
        "你是强盗，你可以选择一个玩家与其交换身份。请输入你想交换的玩家编号：",
    )
    .await;

    if let Some(choice) = receive_player_input(robber.num, game_state.clone()).await {
        if let Ok(target_id) = choice.trim().parse::<usize>() {
            // 拆成两个阶段：先提取数据，再修改
            let (target_role, old_robber_role) = {
                let state = game_state.lock().await;
                let target = state.players.get(&target_id).cloned();
                let robber_self = state.players.get(&robber.num).cloned();

                match (target, robber_self) {
                    (Some(t), Some(r)) => (t.role.clone(), r.role.clone()),
                    _ => {
                        send_private_message(
                            game_state.clone(),
                            robber.num,
                            "找不到该玩家，未进行交换。",
                        )
                        .await;
                        return;
                    }
                }
            };

            {
                let mut state = game_state.lock().await;
                if let Some(player) = state.players.get_mut(&target_id) {
                    player.role = old_robber_role.clone();
                }
                if let Some(robber_player) = state.players.get_mut(&robber.num) {
                    robber_player.role = target_role.clone();
                }
            }

            send_private_message(
                game_state.clone(),
                robber.num,
                &format!("你偷取了玩家 {} 的身份：{}，这现在是你的新身份。", target_id, target_role),
            )
            .await;
        } else {
            send_private_message(
                game_state.clone(),
                robber.num,
                "输入无效，请输入数字。",
            )
            .await;
        }
    }
}


async fn handle_troublemaker(game_state: Arc<Mutex<GameState>>) {
    let troublemakers: Vec<Player>;
    
    {
        let state = game_state.lock().await;
        troublemakers = state.players.values()
            .filter(|p| p.role == "捣蛋鬼")
            .cloned()
            .collect();
    }
    
    if !troublemakers.is_empty() {
        let troublemaker = &troublemakers[0];
        send_private_message(
            game_state.clone(), 
            troublemaker.num, 
            "你是捣蛋鬼，可以选择两名其他玩家交换他们的身份。请输入两个玩家编号（用空格分隔）："
        ).await;
        
        // 获取捣蛋鬼的选择
        if let Some(choice) = receive_player_input(troublemaker.num, game_state.clone()).await {
            let ids: Vec<&str> = choice.trim().split_whitespace().collect();
            if ids.len() == 2 {
                if let (Ok(id1), Ok(id2)) = (ids[0].parse::<usize>(), ids[1].parse::<usize>()) {
                    let success = {
                        let mut state = game_state.lock().await;
                        
                        // 检查两个玩家是否都存在
                        if state.players.contains_key(&id1) && state.players.contains_key(&id2) {
                            // 交换角色
                            let role1 = state.players.get(&id1).unwrap().role.clone();
                            let role2 = state.players.get(&id2).unwrap().role.clone();
                            
                            if let Some(player1) = state.players.get_mut(&id1) {
                                player1.role = role2.clone();
                            }
                            
                            if let Some(player2) = state.players.get_mut(&id2) {
                                player2.role = role1.clone();
                            }
                            
                            true
                        } else {
                            false
                        }
                    };
                    
                    if success {
                        send_private_message(
                            game_state.clone(),
                            troublemaker.num,
                            &format!("你交换了玩家 {} 和玩家 {} 的身份。", id1, id2)
                        ).await;
                    } else {
                        send_private_message(
                            game_state.clone(),
                            troublemaker.num,
                            "找不到指定的一个或两个玩家，未进行交换。"
                        ).await;
                    }
                } else {
                    send_private_message(
                        game_state.clone(),
                        troublemaker.num,
                        "输入无效，请输入两个数字，用空格分隔。"
                    ).await;
                }
            } else {
                send_private_message(
                    game_state.clone(),
                    troublemaker.num,
                    "请输入恰好两个玩家编号，用空格分隔。"
                ).await;
            }
        }
    }
}

async fn handle_drunk(game_state: Arc<Mutex<GameState>>) {
    let drunks: Vec<Player> = {
        let state = game_state.lock().await;
        state.players
            .values()
            .filter(|p| p.role == "酒鬼")
            .cloned()
            .collect()
    };

    if drunks.is_empty() {
        return;
    }

    let drunk = &drunks[0];

    send_private_message(
        game_state.clone(),
        drunk.num,
        "你是酒鬼，你会与一张中心牌交换身份，但你不知道是哪一张。",
    )
    .await;

    // 拆分为两次锁定，避免多次可变 borrow
    let (card_index, old_role, drunk_name) = {
        let mut state = game_state.lock().await;
        if state.center_cards.is_empty() {
            return;
        }

        let card_index = thread_rng().gen_range(0..state.center_cards.len());
        let center_card = state.center_cards[card_index].clone();

        if let Some(player) = state.players.get_mut(&drunk.num) {
            let old_role = player.role.clone();
            player.role = center_card;
            (card_index, old_role, player.name.clone())
        } else {
            return;
        }
    };

    // 再次锁定以修改 center_cards，避免 borrow 冲突
    {
        let mut state = game_state.lock().await;
        state.center_cards[card_index] = old_role.clone();
    }

    println!(
        "酒鬼 {} 与中心牌交换身份，新身份为 {}",
        drunk_name, old_role
    );
}


async fn handle_insomniac(game_state: Arc<Mutex<GameState>>) {
    let insomniacs: Vec<Player>;
    
    {
        let state = game_state.lock().await;
        insomniacs = state.players.values()
            .filter(|p| p.role == "失眠者")
            .cloned()
            .collect();
    }
    
    for insomniac in insomniacs {
        let role;
        {
            let state = game_state.lock().await;
            role = state.players.get(&insomniac.num).map(|p| p.role.clone()).unwrap_or_default();
        }
        
        send_private_message(
            game_state.clone(), 
            insomniac.num, 
            &format!("你是失眠者，你的最终身份是：{}", role)
        ).await;
    }
}