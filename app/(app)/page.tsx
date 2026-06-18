import { redirect } from 'next/navigation'

export default function Home() {
  // 「主页」已下线，根地址默认进入直播管理。
  redirect('/streamers')
}
